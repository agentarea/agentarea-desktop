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
import { cloudList, cloudRead, cloudRef, SIGN_IN } from './cloudFiles';
import { cleanRoot } from './cloudPaths';

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
}

/**
 * Plugin list shared by the Plugins panel and the sidebar, plus navigation: a
 * plugin's sidebar can ask to open its main page (with a #hash) in the panel.
 */
interface PluginUi {
  plugins: Plugin[];
  reload: () => Promise<void>;
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
  void usePlugins.getState().reload();
}

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
