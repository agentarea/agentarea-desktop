/**
 * AgentArea REST client. Uses Tauri's http (no CORS) with the Bearer token from
 * the session. Endpoints verified against the running backend OpenAPI.
 */
import { config } from './config';
import { freshAccessToken } from './session';

/** Active workspace (org) for every request; null = the user's default. */
let workspaceRef: string | null = null;
export function setWorkspaceRef(ref: string | null) {
  workspaceRef = ref;
}
export function getWorkspaceRef(): string | null {
  return workspaceRef;
}
import type { CatalogItem } from '../data';

async function apiFetch(path: string, init?: RequestInit): Promise<Response> {
  const res = await apiRequest(path, init);
  if (!res.ok) throw new Error(`${init?.method ?? 'GET'} ${path} failed (${res.status})`);
  return res;
}

/**
 * Like apiFetch, but hands back non-2xx responses instead of throwing. The
 * token is renewed before it expires; a 401 gets one forced refresh and retry.
 */
export async function apiRequest(path: string, init?: RequestInit): Promise<Response> {
  const res = await send(path, init, await freshAccessToken());
  if (res.status !== 401) return res;
  const renewed = await freshAccessToken(true);
  return renewed ? send(path, init, renewed) : res;
}

async function send(path: string, init: RequestInit | undefined, token: string | null): Promise<Response> {
  const { fetch } = await import('@tauri-apps/plugin-http');
  if (import.meta.env.DEV) console.debug(`[api] ${init?.method ?? 'GET'} ${path} workspace=${workspaceRef ?? '(default)'}`);
  return fetch(`${config.apiBaseUrl}${path}`, {
    ...init,
    headers: {
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...(init?.headers ?? {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
      // The API picks the workspace from this header (id or slug) and checks membership.
      // (In dev, every request logs which workspace it asked for.)
      ...(workspaceRef ? { 'X-AgentArea-Workspace': workspaceRef } : {}),
    },
  });
}

export interface Workspace {
  id: string;
  slug: string;
  name: string;
}

/** GET /v1/workspaces — every workspace (org) the user belongs to. */
export async function listWorkspaces(): Promise<Workspace[]> {
  const raw: any[] = await (await apiFetch('/v1/workspaces')).json();
  return raw.map((w) => ({ id: String(w.id), slug: String(w.slug ?? w.id), name: String(w.name ?? w.slug ?? w.id) }));
}

function items(raw: any): any[] {
  return Array.isArray(raw) ? raw : (raw.items ?? raw.agents ?? raw.data ?? []);
}

function toCatalogItem(x: any): CatalogItem {
  return { id: String(x.id), name: x.name ?? x.display_name ?? String(x.id), description: x.description ?? undefined };
}

/** GET /v1/agents/ — the cloud agents a session can delegate to. */
export async function listAgents(): Promise<CatalogItem[]> {
  return items(await (await apiFetch('/v1/agents/')).json()).map(toCatalogItem);
}

/** First icon URL declared by a registry ServerJSON spec (`icons[0].src`). */
function firstIcon(spec: any): string | undefined {
  const src = Array.isArray(spec?.icons) ? spec.icons[0]?.src : undefined;
  return typeof src === 'string' && src ? src : undefined;
}

/** `https://host/favicon.ico` for the service an endpoint points at (api./mcp. stripped). */
function favicon(url: unknown): string | undefined {
  if (typeof url !== 'string') return undefined;
  try {
    const host = new URL(url).hostname;
    const site = host.replace(/^(api|www|mcp)\./, '');
    return `https://${site.includes('.') ? site : host}/favicon.ico`;
  } catch {
    return undefined;
  }
}

/**
 * GET /v1/mcp-server-instances/ with logos resolved the way the web app does:
 * the instance's own registry icon, its server spec's icon, then the favicon
 * of the host it connects to. The UI falls back to initials.
 */
export async function listMcpInstances(): Promise<CatalogItem[]> {
  const [instances, specs] = await Promise.all([
    apiFetch('/v1/mcp-server-instances/').then((r) => r.json()).then(items),
    // Specs only add icons; a failure here must not hide the instances.
    apiFetch('/v1/mcp-servers/?page_size=100')
      .then((r) => r.json())
      .then(items)
      .catch(() => [] as any[]),
  ]);
  const specById = new Map(specs.map((s: any) => [String(s.id), s]));
  return instances.map((inst: any) => {
    const spec: any = specById.get(String(inst.server_spec_id));
    const endpoint = inst.endpoint_url ?? inst.json_spec?.remotes?.[0]?.url ?? spec?.remote_url;
    const icons = [firstIcon(inst.json_spec), firstIcon(spec?.json_spec), favicon(endpoint)].filter(
      (x): x is string => !!x,
    );
    return { ...toCatalogItem(inst), icons };
  });
}

/**
 * GET /v1/skills. The API merges the workspace's own skills with the platform's
 * built-in catalog, which is the same in every workspace; `builtin` marks those.
 * A non-catalog skill from another workspace would be a scoping bug, so it is
 * dropped (and logged) rather than shown.
 */
export async function listSkills(): Promise<CatalogItem[]> {
  const raw = items(await (await apiFetch('/v1/skills?page_size=100')).json());
  return raw
    .filter((s: any) => {
      const foreign = !s.is_catalog && workspaceRef && s.workspace_id && s.workspace_id !== workspaceRef;
      if (foreign) console.warn('skill from another workspace dropped', s.id, s.workspace_id);
      return !foreign;
    })
    .map((s: any) => ({ ...toCatalogItem(s), builtin: !!s.is_catalog }));
}

export interface Client {
  id: string;
  mcpEndpointUrl: string;
}

/** POST /v1/clients/ — one client per desktop session. */
export async function createClient(name: string): Promise<Client> {
  const res = await apiFetch('/v1/clients/', {
    method: 'POST',
    body: JSON.stringify({ name, kind: 'desktop' }),
  });
  const c = await res.json();
  return { id: String(c.id), mcpEndpointUrl: c.mcp_endpoint_url };
}

export async function deleteClient(clientId: string): Promise<void> {
  await apiFetch(`/v1/clients/${clientId}`, { method: 'DELETE' });
}

type Member = 'mcp-instances' | 'skills';

export async function addClientMember(clientId: string, member: Member, id: string): Promise<void> {
  await apiFetch(`/v1/clients/${clientId}/${member}`, { method: 'POST', body: JSON.stringify({ id }) });
}

export async function removeClientMember(clientId: string, member: Member, id: string): Promise<void> {
  await apiFetch(`/v1/clients/${clientId}/${member}/${id}`, { method: 'DELETE' });
}

// ── MCP Apps ─────────────────────────────────────────────────────────────────
// Tools whose MCP server ships a UI resource (`ui://…`), rendered by McpAppFrame.

export interface McpApp {
  instance_id: string;
  instance_name: string;
  tool_name: string;
  title: string | null;
  description: string;
  resource_uri: string;
  /** the entry tool has required arguments; the host has no form for them yet */
  requires_input: boolean;
}

export interface McpAppResource {
  uri: string;
  mime_type: string;
  html: string;
  csp: Record<string, unknown> | null;
  permissions: Record<string, unknown> | null;
  prefers_border: boolean | null;
}

export interface McpAppToolCall {
  name: string;
  arguments: Record<string, unknown>;
  /** "host" for the entry call, "app" for calls the app makes itself */
  caller: 'host' | 'app';
}

export interface McpAppToolCallResult {
  content: unknown[];
  structured_content: unknown;
  is_error: boolean;
}

/** Like apiFetch + json(), but the error carries the API's `detail` message. */
async function apiJson<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await apiRequest(path, init);
  if (!res.ok) {
    let detail = '';
    try {
      const body = await res.json();
      if (typeof body?.detail === 'string') detail = body.detail;
    } catch {
      /* non-JSON error body */
    }
    throw new Error(detail || `${init?.method ?? 'GET'} ${path} failed (${res.status})`);
  }
  return res.json();
}

/** GET /v1/mcp-apps/ */
export async function listMcpApps(): Promise<McpApp[]> {
  const apps = await apiJson<McpApp[]>('/v1/mcp-apps/');
  return apps.map((a) => ({ ...a, instance_id: String(a.instance_id) }));
}

/** GET /v1/mcp-apps/{instance_id}/resource?uri= — the app's HTML + CSP. */
export async function readMcpAppResource(instanceId: string, uri: string): Promise<McpAppResource> {
  const q = new URLSearchParams({ uri });
  return apiJson(`/v1/mcp-apps/${encodeURIComponent(instanceId)}/resource?${q}`);
}

/** POST /v1/mcp-apps/{instance_id}/tools/call */
export async function callMcpAppTool(instanceId: string, call: McpAppToolCall): Promise<McpAppToolCallResult> {
  return apiJson(`/v1/mcp-apps/${encodeURIComponent(instanceId)}/tools/call`, {
    method: 'POST',
    body: JSON.stringify(call),
  });
}

/** A cloud task that wants the user: approval, input, a failure, or (optionally) a result. */
export interface CloudInboxItem {
  taskId: string;
  agentId: string;
  agentName: string | null;
  description: string;
  status: 'waiting_for_approval' | 'waiting_for_input' | 'failed' | 'completed' | string;
  createdAt: string;
  escalationId: string | null;
  escalationTool: string | null;
  error: string | null;
}

/** GET /v1/inbox — the workspace's tasks needing attention, newest first. */
export async function listInbox(): Promise<CloudInboxItem[]> {
  const raw = await (await apiFetch('/v1/inbox/?page_size=100')).json();
  return items(raw).map((t: any) => ({
    taskId: String(t.id),
    agentId: String(t.agent_id),
    agentName: t.agent_name ?? null,
    description: String(t.description ?? ''),
    status: String(t.status),
    createdAt: String(t.created_at),
    escalationId: t.escalation_id ?? null,
    escalationTool: t.escalation_tool_name ?? null,
    error: t.error ?? t.failure_reason ?? null,
  }));
}

/** Approve or deny a cloud task's pending tool call. */
export async function resolveEscalation(item: CloudInboxItem, approved: boolean): Promise<void> {
  await apiFetch(`/v1/agents/${item.agentId}/tasks/${item.taskId}/resolve-escalation`, {
    method: 'POST',
    body: JSON.stringify({ escalation_id: item.escalationId, approved, comment: null }),
  });
}
