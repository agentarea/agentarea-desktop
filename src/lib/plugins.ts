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
  /** extra bridge powers: "fs" = read files in a folder the user grants,
   *  "cloud" = read the AgentArea workspace's files under `cloudRoot` */
  permissions: string[];
  /** the granted folder, for "fs" plugins */
  root: string | null;
  /** workspace-files prefix for "cloud" plugins: "" or "a/b/" (default "wiki/") */
  cloudRoot: string | null;
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

/** The plugin's cloud root, if it is enabled and declared "cloud". */
function cloudRoot(pluginId: string): string {
  const p = usePlugins.getState().plugins.find((x) => x.id === pluginId);
  if (!p?.enabled || !p.permissions.includes('cloud')) throw new Error(`plugin ${pluginId} has no cloud access`);
  return p.cloudRoot ?? 'wiki/';
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
      return { path: usePlugins.getState().plugins.find((p) => p.id === pluginId)?.root ?? null };

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

    // ── threads
    case 'threads.attach': {
      // Attach a file from the plugin's folder to a thread (default: the open one).
      const root = usePlugins.getState().plugins.find((p) => p.id === pluginId)?.root;
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
      throw new Error(`Unknown method: ${method}`);
  }
}
