/**
 * MCP servers on this device (see src-tauri/src/local_mcp.rs): installed ones
 * are data in `~/AgentArea/mcp/servers.json`, running ones are stdio children
 * of the app. Works without an AgentArea sign-in.
 *
 * A server's MCP Apps are found the way the web app finds cloud ones: tools
 * whose `_meta.ui.resourceUri` points at a `ui://` resource.
 */
import { invoke } from '@tauri-apps/api/core';
import { create } from 'zustand';
import type { McpAppResource } from '@/lib/api';
import { toCallToolResult, type CallAppTool } from '@/lib/mcp-apps/tool-result';

export type LocalSource = 'recommended' | 'registry' | 'custom';

/** One entry of servers.json. */
export interface LocalServerSpec {
  id: string;
  name: string;
  description?: string;
  icons?: string[];
  source: LocalSource;
  /** registry base URL it was installed from */
  registry?: string;
  /** stdio server; empty when `url` is set */
  command: string;
  args: string[];
  env?: Record<string, string>;
  /** a server already listening: its streamable-HTTP MCP endpoint */
  url?: string;
  headers?: Record<string, string>;
}

export interface LocalServer extends LocalServerSpec {
  /** "error" = it exited on its own; `stderr` says why */
  status: 'stopped' | 'running' | 'error';
  stderr: string | null;
}

export interface LocalTool {
  name: string;
  title?: string;
  description?: string;
  inputSchema?: { required?: string[] };
  _meta?: Record<string, unknown>;
}

export interface LocalApp {
  serverId: string;
  serverName: string;
  icons?: string[];
  toolName: string;
  title: string;
  description?: string;
  resourceUri: string;
  /** the entry tool has required arguments; there is no form for them yet */
  requiresInput: boolean;
}

export type ServerState = 'stopped' | 'starting' | 'running' | 'error';

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function record(v: unknown): Record<string, unknown> | undefined {
  return typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
}

export function localRequest<T = unknown>(id: string, method: string, params?: unknown): Promise<T> {
  return invoke<T>('local_mcp_request', { id, method, params: params ?? {} });
}

/** Every tool, following `nextCursor` (bounded, in case a server loops). */
async function listTools(id: string): Promise<LocalTool[]> {
  const tools: LocalTool[] = [];
  let cursor: string | undefined;
  for (let page = 0; page < 20; page++) {
    const res = await localRequest<{ tools?: LocalTool[]; nextCursor?: string }>(id, 'tools/list', cursor ? { cursor } : {});
    tools.push(...(res.tools ?? []));
    cursor = res.nextCursor;
    if (!cursor) break;
  }
  return tools;
}

/** `_meta.ui.resourceUri`, or the older flat `_meta["ui/resourceUri"]`. */
function uiResourceUri(tool: LocalTool): string | null {
  const uri = record(record(tool._meta)?.ui)?.resourceUri ?? record(tool._meta)?.['ui/resourceUri'];
  return typeof uri === 'string' && uri.startsWith('ui://') ? uri : null;
}

/** Who may call a tool; MCP Apps default to both the model and the app. */
function visibility(tool: LocalTool): string[] {
  const v = record(record(tool._meta)?.ui)?.visibility;
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : ['model', 'app'];
}

/** A server's apps: UI tools the model may open (app-only helpers are not entries). */
export function appsOf(server: LocalServerSpec, tools: LocalTool[]): LocalApp[] {
  return tools.flatMap((tool) => {
    const resourceUri = uiResourceUri(tool);
    if (!resourceUri || !visibility(tool).includes('model')) return [];
    return [
      {
        serverId: server.id,
        serverName: server.name,
        icons: server.icons,
        toolName: tool.name,
        title: tool.title || tool.name,
        description: tool.description,
        resourceUri,
        requiresInput: (tool.inputSchema?.required ?? []).length > 0,
      },
    ];
  });
}

/** UTF-8 text of a base64 `blob`. */
function decodeBase64(b64: string): string {
  const bin = atob(b64);
  return new TextDecoder().decode(Uint8Array.from(bin, (c) => c.charCodeAt(0)));
}

/** `resources/read` → the shape McpAppFrame takes (same as the cloud API's). */
export async function readLocalAppResource(serverId: string, uri: string): Promise<McpAppResource> {
  const res = await localRequest<{ contents?: unknown[] }>(serverId, 'resources/read', { uri });
  const content = record(res.contents?.[0]);
  if (!content) throw new Error(`${uri} returned no content`);
  const html =
    typeof content.text === 'string' ? content.text : typeof content.blob === 'string' ? decodeBase64(content.blob) : null;
  if (html === null) throw new Error(`${uri} has no HTML`);
  const ui = record(record(content._meta)?.ui);
  return {
    uri,
    mime_type: typeof content.mimeType === 'string' ? content.mimeType : 'text/html;profile=mcp-app',
    html,
    csp: record(ui?.csp) ?? null,
    permissions: record(ui?.permissions) ?? null,
    prefers_border: typeof ui?.prefersBorder === 'boolean' ? ui.prefersBorder : null,
  };
}

/** Tool calls for an app on this device; an app may call only tools visible to apps. */
export function localToolCaller(serverId: string): CallAppTool {
  return async ({ name, arguments: args, caller }) => {
    const tool = useLocalMcp.getState().tools[serverId]?.find((t) => t.name === name);
    if (caller === 'app' && tool && !visibility(tool).includes('app')) {
      throw new Error(`${name} is not available to apps`);
    }
    return toCallToolResult(await localRequest<Record<string, unknown>>(serverId, 'tools/call', { name, arguments: args }));
  };
}

/** Installed servers, their running state and discovered tools. */
interface LocalMcpUi {
  servers: LocalServer[];
  /** tools/list per running server */
  tools: Record<string, LocalTool[]>;
  starting: Record<string, boolean>;
  /** start or tools/list failures, per server */
  errors: Record<string, string>;
  /** set when the list itself can't be read (e.g. outside Tauri) */
  listError: string | null;
  reload: () => Promise<void>;
  start: (id: string) => Promise<void>;
  stop: (id: string) => Promise<void>;
  /** install (or update) and start */
  install: (spec: LocalServerSpec) => Promise<void>;
  uninstall: (id: string) => Promise<void>;
}

const omit = <T,>(map: Record<string, T>, key: string) => {
  const { [key]: _, ...rest } = map;
  return rest;
};

export const useLocalMcp = create<LocalMcpUi>((set, get) => {
  const discover = async (id: string) => {
    try {
      const tools = await listTools(id);
      set((s) => ({ tools: { ...s.tools, [id]: tools }, errors: omit(s.errors, id) }));
    } catch (e) {
      set((s) => ({ errors: { ...s.errors, [id]: errorText(e) } }));
    }
  };

  return {
    servers: [],
    tools: {},
    starting: {},
    errors: {},
    listError: null,

    reload: async () => {
      try {
        const servers = await invoke<LocalServer[]>('local_mcp_list');
        const running = new Set(servers.filter((s) => s.status === 'running').map((s) => s.id));
        set((s) => ({
          servers,
          listError: null,
          tools: Object.fromEntries(Object.entries(s.tools).filter(([id]) => running.has(id))),
        }));
        await Promise.all([...running].filter((id) => !get().tools[id]).map(discover));
      } catch (e) {
        set({ listError: errorText(e) });
      }
    },

    start: async (id) => {
      if (get().starting[id]) return;
      set((s) => ({ starting: { ...s.starting, [id]: true }, errors: omit(s.errors, id) }));
      try {
        await invoke('local_mcp_start', { id });
        await discover(id);
      } catch (e) {
        set((s) => ({ errors: { ...s.errors, [id]: errorText(e) } }));
      } finally {
        set((s) => ({ starting: omit(s.starting, id) }));
        await get().reload();
      }
    },

    stop: async (id) => {
      await invoke('local_mcp_stop', { id });
      set((s) => ({ tools: omit(s.tools, id), errors: omit(s.errors, id) }));
      await get().reload();
    },

    install: async (spec) => {
      // A reinstall may change the command: restart it from the new spec.
      if (get().servers.some((s) => s.id === spec.id && s.status === 'running')) await get().stop(spec.id);
      await invoke('local_mcp_install', { server: spec });
      await get().reload();
      void get().start(spec.id);
    },

    uninstall: async (id) => {
      await invoke('local_mcp_uninstall', { id });
      set((s) => ({ tools: omit(s.tools, id), errors: omit(s.errors, id) }));
      await get().reload();
    },
  };
});

/** What the UI shows for a server: a start in flight wins over the last status. */
export function serverState(server: LocalServer, starting: boolean, error: string | undefined): ServerState {
  if (starting) return 'starting';
  if (server.status === 'running') return error ? 'error' : 'running';
  return server.status === 'error' || error ? 'error' : 'stopped';
}
