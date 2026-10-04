/**
 * Local plugins: folders in `~/AgentArea/plugins/<id>/` (see
 * src-tauri/src/plugins.rs), rendered in PluginFrame. A plugin reaches the app
 * only through this bridge — postMessage requests answered from the store:
 *
 *   plugin → host  { type: "aa:request",  id, method, params }
 *   host → plugin  { type: "aa:response", id, result } | { …, error }
 */
import { invoke } from '@tauri-apps/api/core';
import { create } from 'zustand';
import { useAppStore } from '@/store';
import { listWorkspaceMcpInstances } from './api';
import { cloudList, cloudRead, cloudRef, SIGN_IN } from './cloudFiles';
import { cleanRoot } from './cloudPaths';
import { config } from './config';

/** Where a plugin's MCP backend should run (plugins-state.json; default auto). */
export type PlacementMode = 'auto' | 'local' | 'cloud';

/** plugin.json `server` (src-tauri/src/plugin_backends.rs); env is not sent. */
export interface PluginServer {
  /** a stdio MCP server started from the plugin folder */
  local: { command: string[] } | null;
  /** the AgentArea workspace's MCP instance named `plugin:<id>` */
  cloud: { instance: string } | null;
}

/** What the user approves (`plugin_server_review`): exactly what runs, where, with what. */
export interface ServerReview {
  command: string[] | null;
  /** where the program resolves on this machine */
  executable: string | null;
  /** the plugin folder reviewed */
  cwd: string;
  /** the read-only copy it runs from once approved */
  runsFrom: string;
  /** the local server's extra environment, values included */
  env: Record<string, string>;
  warnings: string[];
  cloud: {
    instance: string;
    instanceId: string | null;
    workspace: string | null;
    /** what approving binds; passed back to approve */
    binding: string | null;
    approved: boolean;
  } | null;
  files: number;
  bytes: number;
  /** SHA-256 over the plugin folder as reviewed; passed back to approve */
  fingerprint: string;
  /** this exact version (and cloud instance, if any) is approved */
  approved: boolean;
  hasApproval: boolean;
}

/** A backend's placement in the active workspace. */
export interface BackendStatus {
  id: string;
  mode: PlacementMode;
  resolved: 'local' | 'cloud' | 'unavailable';
  reason?: string;
  /** the matched cloud instance id */
  instance?: string;
  /** something to review, e.g. a cloud instance that appeared */
  notice?: string;
}

export interface Plugin {
  id: string;
  title: string;
  description: string | null;
  /** entry page, relative to the plugin folder */
  entry: string;
  /** off = not shown or served; state kept in ~/AgentArea/plugins-state.json */
  enabled: boolean;
  /** optional page shown as a section in the app sidebar */
  sidebar: string | null;
  /** extra bridge powers: "fs" = read (and reveal in Finder) files in a folder the user grants,
   *  "cloud" = read the AgentArea workspace's files under `cloudRoot`,
   *  "state" = the Channels network database (state.*) and creating vault secrets (vault.*) */
  permissions: string[];
  /** the granted folder, for "fs" plugins */
  root: string | null;
  /** workspace-files prefix for "cloud" plugins: "" or "a/b/" (default "wiki/") */
  cloudRoot: string | null;
  /** plugin.json `icon`: a lucide name ("radio-tower"), or the plugin's own SVG as a
   *  `data:` URL (validated and size-capped in Rust); null = initials badge */
  icon: string | null;
  /** MCP backend, reachable at one local gateway URL wherever it runs */
  server: PluginServer | null;
  /** why plugin.json's `server` was not accepted */
  serverError: string | null;
  /** placement the user picked; null without a server */
  placement: PlacementMode | null;
  /** a version of the backend is approved (it runs from its read-only copy) */
  serverApproved: boolean;
  /** the folder changed since: review again to switch versions */
  updateAvailable: boolean;
  /** an approval entry that no longer counts (Revoke clears it) */
  staleApproval: boolean;
}

/**
 * Plugin list shared by the Plugins panel and the sidebar, plus navigation: a
 * plugin's sidebar can ask to open its main page (with a #hash) in the panel.
 */
interface PluginUi {
  plugins: Plugin[];
  reload: () => Promise<void>;
  /** backend placements for the active workspace, by plugin id */
  backends: Record<string, BackendStatus>;
  reloadBackends: () => Promise<void>;
  /** set by `ui.open`; the Plugins panel selects it and App switches to it */
  target: { id: string; hash: string; at: number } | null;
  open: (id: string, hash: string) => void;
}

export const usePlugins = create<PluginUi>((set) => ({
  plugins: [],
  reload: async () => {
    try {
      set({ plugins: await listPlugins() });
    } catch {
      /* not in Tauri */
    }
  },
  backends: {},
  reloadBackends: async () => {
    try {
      const list = await invoke<BackendStatus[]>('plugin_backend_status');
      set({ backends: Object.fromEntries(list.map((b) => [b.id, b])) });
    } catch {
      /* not in Tauri */
    }
  },
  target: null,
  open: (id, hash) => set({ target: { id, hash, at: Date.now() } }),
}));

export interface BridgeRequest {
  type: 'aa:request';
  id: string | number;
  method: string;
  params?: unknown;
}

export interface BridgeResponse {
  type: 'aa:response';
  id: string | number;
  result?: unknown;
  error?: string;
}

/** Installed plugins; the first call seeds an example one. */
export function listPlugins(): Promise<Plugin[]> {
  return invoke<Plugin[]>('list_plugins');
}

export async function setPluginEnabled(id: string, enabled: boolean): Promise<void> {
  await invoke('set_plugin_enabled', { id, enabled });
  await usePlugins.getState().reload();
  void usePlugins.getState().reloadBackends();
}

export async function setPluginPlacement(id: string, mode: PlacementMode): Promise<void> {
  await invoke('plugin_set_placement', { id, mode });
  await usePlugins.getState().reload();
  void usePlugins.getState().reloadBackends();
}

/** The plugin's backend as it would be approved now (reads every file). */
export function reviewPluginServer(id: string): Promise<ServerReview> {
  return invoke<ServerReview>('plugin_server_review', { id });
}

/**
 * Allow the backend exactly as reviewed (`fingerprint` from the review; Rust
 * refuses if the folder changed since), or revoke.
 */
export async function approvePluginServer(id: string, approve: boolean, review?: ServerReview): Promise<void> {
  await invoke('plugin_approve_server', {
    id,
    approve,
    fingerprint: review?.fingerprint ?? null,
    binding: review?.cloud?.binding ?? null,
  });
  await usePlugins.getState().reload();
  void usePlugins.getState().reloadBackends();
}

/**
 * Tell Rust which workspace is active — env, slug, API base and its MCP
 * instances — so plugin backends resolve their placement against it. The
 * token is not sent: Rust reads the live one (session.ts keeps it fresh).
 * Runs on start and whenever the sign-in or workspace changes.
 */
let syncSeq = 0;
export async function syncBackendContext(): Promise<void> {
  const seq = ++syncSeq;
  const s = useAppStore.getState();
  const ws = s.workspaces.find((w) => w.id === s.workspaceId) ?? null;
  const signedIn = s.auth.status === 'authenticated';
  let instances: { id: string; name: string }[] | null = null;
  let error: string | null = null;
  if (signedIn && ws) {
    try {
      instances = await listWorkspaceMcpInstances(ws.slug);
    } catch (e) {
      error = `couldn't list the workspace's MCP instances: ${e instanceof Error ? e.message : String(e)}`;
    }
  } else if (signedIn) {
    error = 'no AgentArea workspace selected yet';
  }
  if (seq !== syncSeq) return; // a newer sync is on its way
  try {
    await invoke('plugin_backends_set_context', {
      env: s.env.id,
      slug: ws?.slug ?? null,
      apiBase: config.apiBaseUrl,
      instances,
      error,
    });
  } catch {
    return; // not in Tauri
  }
  await usePlugins.getState().reloadBackends();
}

let syncTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleSync() {
  if (syncTimer) clearTimeout(syncTimer);
  syncTimer = setTimeout(() => void syncBackendContext(), 200);
}

// After this module's importers finish loading (the store may not exist yet).
queueMicrotask(() => {
  scheduleSync();
  useAppStore.subscribe((s, prev) => {
    if (s.env !== prev.env || s.auth.status !== prev.auth.status || s.workspaceId !== prev.workspaceId || s.workspaces !== prev.workspaces) {
      scheduleSync();
    }
  });
});

export function isBridgeRequest(data: unknown): data is BridgeRequest {
  if (typeof data !== 'object' || data === null) return false;
  const m = data as Record<string, unknown>;
  return (
    m.type === 'aa:request' &&
    (typeof m.id === 'string' || typeof m.id === 'number') &&
    typeof m.method === 'string'
  );
}

function param(params: unknown, key: string): unknown {
  return typeof params === 'object' && params !== null ? (params as Record<string, unknown>)[key] : undefined;
}

function str(params: unknown, key: string): string {
  const v = param(params, key);
  if (typeof v !== 'string') throw new Error(`"${key}" must be a string`);
  return v;
}

/** Throws unless the plugin is enabled and declared "state" (re-reads the list once: manifests change on disk). */
async function requireState(pluginId: string): Promise<void> {
  const ok = () => {
    const p = usePlugins.getState().plugins.find((x) => x.id === pluginId);
    return !!p?.enabled && p.permissions.includes('state');
  };
  if (!ok()) await usePlugins.getState().reload();
  if (!ok()) throw new Error(`plugin ${pluginId} has no "state" permission`);
}

/** Throws unless the plugin is enabled and declares a server: it reaches only its own backend. */
async function requireServer(pluginId: string): Promise<void> {
  const ok = () => {
    const p = usePlugins.getState().plugins.find((x) => x.id === pluginId);
    return !!p?.enabled && !!p.server;
  };
  if (!ok()) await usePlugins.getState().reload();
  if (!ok()) throw new Error(`plugin ${pluginId} has no backend`);
}

/** The plugin's cloud root, if it is enabled and declared "cloud". */
function cloudRoot(pluginId: string): string {
  const p = usePlugins.getState().plugins.find((x) => x.id === pluginId);
  if (!p?.enabled || !p.permissions.includes('cloud')) throw new Error(`plugin ${pluginId} has no cloud access`);
  return p.cloudRoot ?? 'wiki/';
}

/**
 * The plugin with its granted folder. The list is loaded once, but agents add
 * plugins and write `plugins-state.json` while the app runs, so a plugin that
 * is missing or has no folder yet triggers one re-read before giving up.
 */
async function withRoot(pluginId: string): Promise<Plugin | undefined> {
  const find = () => usePlugins.getState().plugins.find((p) => p.id === pluginId);
  if (!find()?.root) await usePlugins.getState().reload();
  return find();
}

/** Run one bridge method for `pluginId` against the app store. Throws on bad input. */
export async function runBridgeMethod(pluginId: string, method: string, params: unknown): Promise<unknown> {
  const store = useAppStore.getState();
  switch (method) {
    // ── cloud: the AgentArea workspace's files, for plugins that declared "cloud".
    // Paths are relative to the plugin's cloud root; HTTP goes through apiRequest.
    case 'cloud.info': {
      const root = cloudRoot(pluginId);
      const connected = store.auth.status === 'authenticated';
      const ws = store.workspaces.find((w) => w.id === store.workspaceId);
      return { connected, workspace: connected && ws ? { id: ws.id, name: ws.name } : null, root };
    }

    case 'cloud.list': {
      const root = cloudRoot(pluginId);
      const prefix = param(params, 'prefix') ?? param(params, 'path') ?? '';
      if (typeof prefix !== 'string') throw new Error('"prefix" must be a string');
      return cloudList(root, prefix);
    }

    case 'cloud.read':
      return cloudRead(cloudRoot(pluginId), str(params, 'path'));

    case 'cloud.setRoot': {
      cloudRoot(pluginId);
      const prefix = cleanRoot(str(params, 'prefix'));
      const root = await invoke<string>('plugin_set_cloud_root', { id: pluginId, prefix });
      await usePlugins.getState().reload();
      return { root };
    }

    // ── files: only for plugins that declared "fs", only inside their folder (Rust enforces)
    case 'fs.root':
      return { path: (await withRoot(pluginId))?.root ?? null };

    case 'fs.pickRoot': {
      const { open } = await import('@tauri-apps/plugin-dialog');
      const dir = await open({ directory: true, multiple: false, title: 'Folder for this plugin' });
      if (typeof dir !== 'string') return { path: null };
      await invoke('plugin_set_root', { id: pluginId, path: dir });
      await usePlugins.getState().reload();
      return { path: dir };
    }

    case 'fs.list':
      return invoke('plugin_fs_list', { id: pluginId, path: typeof param(params, 'path') === 'string' ? param(params, 'path') : '' });

    case 'fs.read':
      return invoke('plugin_fs_read', { id: pluginId, path: str(params, 'path') });

    case 'fs.reveal': {
      // Show the plugin's folder, or an entry inside it, in Finder. Read-only.
      const p = await withRoot(pluginId);
      if (!p?.enabled || !p.permissions.includes('fs')) throw new Error(`plugin ${pluginId} has no file access`);
      if (!p.root) throw new Error('no folder chosen yet');
      const raw = param(params, 'path') ?? '';
      if (typeof raw !== 'string') throw new Error('"path" must be a string');
      const rel = raw.replace(/^\/+|\/+$/g, '');
      if (rel && rel.split('/').some((s) => !s || s === '.' || s === '..' || s.includes('\\'))) throw new Error('bad path');
      const { revealItemInDir } = await import('@tauri-apps/plugin-opener');
      await revealItemInDir(rel ? `${p.root.replace(/\/+$/, '')}/${rel}` : p.root);
      return { ok: true };
    }

    // ── vault: create / list secrets for "state" plugins (inventory credentials).
    // Values go straight to the Keychain; nothing here ever returns one.
    case 'vault.list': {
      await requireState(pluginId);
      const { listSecrets } = await import('./vault');
      return (await listSecrets()).map((s) => ({ name: s.name, description: s.description ?? '', allowedHosts: s.allowedHosts }));
    }

    case 'vault.create': {
      await requireState(pluginId);
      const { createSecret, parseHosts } = await import('./vault');
      const value = str(params, 'value');
      if (!value) throw new Error('"value" is empty');
      const hosts = param(params, 'allowedHosts');
      const s = await createSecret({
        name: str(params, 'name'),
        description: typeof param(params, 'description') === 'string' ? (param(params, 'description') as string) : undefined,
        allowedHosts: Array.isArray(hosts) ? hosts.filter((h): h is string => typeof h === 'string') : parseHosts(typeof hosts === 'string' ? hosts : ''),
        value,
      });
      return { name: s.name, description: s.description ?? '', allowedHosts: s.allowedHosts };
    }

    // ── threads
    case 'threads.attach': {
      // Attach a file from the plugin's folder to a thread (default: the open one).
      const root = (await withRoot(pluginId))?.root;
      if (!root) throw new Error('no folder chosen yet');
      const rel = str(params, 'path').replace(/^\/+/, '');
      if (rel.split('/').includes('..')) throw new Error('bad path');
      const threadId = (param(params, 'threadId') as string | undefined) ?? store.selectedId;
      if (!threadId) throw new Error('No open thread');
      store.referenceFiles(threadId, [`${root.replace(/\/+$/, '')}/${rel}`]);
      return { ok: true, threadId };
    }

    case 'threads.attachCloud': {
      // Attach a workspace file by reference; the agent reads it via agentarea_data.
      const ref = cloudRef(cloudRoot(pluginId), str(params, 'path'));
      if (store.auth.status !== 'authenticated') throw new Error(SIGN_IN);
      const threadId = (param(params, 'threadId') as string | undefined) ?? store.selectedId;
      if (!threadId) throw new Error('No open thread');
      store.referenceFiles(threadId, [ref]);
      return { ok: true, threadId, ref };
    }

    // ── plugin: the plugin's own MCP backend (local or cloud, one gateway in Rust).
    // The id is always the calling frame's plugin (PluginFrame passes it); the
    // frame has no IPC of its own, so it can't name another.
    case 'plugin.placement': {
      await requireServer(pluginId);
      await usePlugins.getState().reloadBackends();
      const b = usePlugins.getState().backends[pluginId];
      if (!b) throw new Error(`plugin ${pluginId} has no backend`);
      return { mode: b.mode, resolved: b.resolved, ...(b.reason ? { reason: b.reason } : {}) };
    }

    case 'plugin.tools':
      await requireServer(pluginId);
      return invoke('plugin_backend_tools', { id: pluginId });

    case 'plugin.call': {
      await requireServer(pluginId);
      const args = param(params, 'arguments') ?? param(params, 'args') ?? {};
      if (typeof args !== 'object' || args === null || Array.isArray(args)) throw new Error('"arguments" must be an object');
      return invoke('plugin_backend_call', { id: pluginId, name: str(params, 'name'), arguments: args });
    }

    // ── ui
    case 'ui.open':
      usePlugins.getState().open(pluginId, typeof param(params, 'hash') === 'string' ? (param(params, 'hash') as string) : '');
      return { ok: true };

    case 'threads.list':
      return store.sessions.map((s) => ({
        id: s.id,
        title: s.title,
        runner: s.runner,
        running: !!store.running[s.id],
      }));

    case 'threads.send': {
      const threadId = param(params, 'threadId');
      const text = param(params, 'text');
      if (typeof threadId !== 'string' || typeof text !== 'string' || !text.trim()) {
        throw new Error('threads.send needs { threadId, text }');
      }
      if (!store.sessions.some((s) => s.id === threadId)) throw new Error('No such thread');
      // `send` silently drops a message while a turn runs; say so instead.
      if (store.running[threadId]) throw new Error('The thread is busy');
      // Not awaited: a cloud turn resolves only when the agent finishes, and
      // failures land in the thread itself.
      void store.send(threadId, text.trim());
      return { ok: true };
    }

    case 'threads.create':
      await store.newSession();
      return { id: useAppStore.getState().selectedId };

    default:
      // ── state: the Channels network database (src-tauri/src/state_db.rs), for
      // plugins that declared "state". Rust checks the permission and audits
      // writes as `plugin:<id>`: state.query {sql, params}, state.snapshot,
      // state.rev, state.inventory_bind, state.channel_update, …
      if (method.startsWith('state.')) {
        return invoke('state_call', { method: method.slice(6), params: params ?? {}, plugin: pluginId });
      }
      throw new Error(`Unknown method: ${method}`);
  }
}
