/**
 * App links: an MCP App opens another UI of its own connection by asking the
 * host to open `agentarea-app://<tool>?<param>=<value>&…` (or
 * `agentarea-app:<tool>`). It rides the open-link request MCP Apps already
 * define, so apps need no protocol extension. Same contract as the web app
 * (agentarea-webapp src/lib/apps/links.ts + openMcpAppLinkAction).
 *
 * Pure: no Tauri, no stores — AppView fetches the connection's tools.
 */
import type { AppRef } from './pinnedApps';

export const APP_LINK_PROTOCOL = 'agentarea-app:';

export interface AppLink {
  toolName: string;
  params: Record<string, string>;
}

/** A tool as `tools/list` (or a cloud instance's stored list) describes it. */
export interface LinkTool {
  name: string;
  title?: string;
  inputSchema?: { properties?: Record<string, unknown>; required?: string[] };
  _meta?: Record<string, unknown>;
}

function record(v: unknown): Record<string, unknown> | undefined {
  return typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
}

/** The target of an app link, or null when the URL is not one. */
export function parseAppLink(url: string): AppLink | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  if (parsed.protocol !== APP_LINK_PROTOCOL) return null;
  // `agentarea-app://tool` puts the name in the host, `agentarea-app:tool` in
  // the path; both are accepted, and nothing else may follow it.
  if (parsed.host && parsed.pathname !== '' && parsed.pathname !== '/') return null;
  let toolName: string;
  try {
    toolName = decodeURIComponent(parsed.host || parsed.pathname);
  } catch {
    return null;
  }
  if (!toolName || toolName.includes('/')) return null;
  return { toolName, params: Object.fromEntries(parsed.searchParams) };
}

/** `_meta.ui.resourceUri`, or the older flat `_meta["ui/resourceUri"]`. */
export function uiResourceUri(tool: LinkTool): string | null {
  const uri = record(record(tool._meta)?.ui)?.resourceUri ?? record(tool._meta)?.['ui/resourceUri'];
  return typeof uri === 'string' && uri.startsWith('ui://') ? uri : null;
}

/** Who may call a tool; MCP Apps default to both the model and the app. */
export function visibility(tool: LinkTool): string[] {
  const v = record(record(tool._meta)?.ui)?.visibility;
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : ['model', 'app'];
}

/**
 * The tool a link opens and its UI, or a thrown reason why not. A link may
 * open only an entry tool (a model-visible tool with a UI) that apps may also
 * call, so a link cannot reach a tool the app could not call itself.
 */
export function resolveLinkTool(tools: readonly unknown[], toolName: string): { tool: LinkTool; resourceUri: string } {
  const tool = tools.find((t): t is LinkTool => record(t)?.name === toolName);
  if (!tool) throw new Error(`Tool "${toolName}" is not exposed by this connection`);
  const seen = visibility(tool);
  if (!seen.includes('app')) throw new Error(`Tool "${toolName}" is not callable from an MCP App`);
  const resourceUri = uiResourceUri(tool);
  if (!resourceUri || !seen.includes('model')) throw new Error(`Tool "${toolName}" is not an MCP App entry point`);
  return { tool, resourceUri };
}

/**
 * Arguments for a tool from a link's query values, which are all strings:
 * each is converted to the type the tool's input schema declares for it. A
 * value that does not convert is refused rather than passed on.
 */
export function linkArguments(tool: LinkTool, params: Readonly<Record<string, string>>): Record<string, unknown> {
  const properties = record(tool.inputSchema?.properties) ?? {};
  return Object.fromEntries(
    Object.entries(params).map(([name, value]) => {
      const type = record(properties[name])?.type;
      if (type === 'integer' || type === 'number') {
        const n = Number(value);
        if (value.trim() === '' || !Number.isFinite(n) || (type === 'integer' && !Number.isInteger(n))) {
          throw new Error(`"${name}" must be ${type === 'integer' ? 'an integer' : 'a number'}`);
        }
        return [name, n];
      }
      if (type === 'boolean') {
        if (value !== 'true' && value !== 'false') throw new Error(`"${name}" must be true or false`);
        return [name, value === 'true'];
      }
      if (type === 'array' || type === 'object') {
        let json: unknown;
        try {
          json = JSON.parse(value);
        } catch {
          json = undefined;
        }
        if (type === 'array' ? !Array.isArray(json) : !record(json)) {
          throw new Error(`"${name}" must be a JSON ${type}`);
        }
        return [name, json];
      }
      return [name, value];
    }),
  );
}

/**
 * The linked view as a normal app ref on the same connection: pinnable,
 * savable in a group. Its key carries the link's parameters, so two lead
 * cards are two views.
 */
export function linkedAppRef(source: AppRef, link: AppLink, tools: readonly unknown[]): AppRef {
  const { tool, resourceUri } = resolveLinkTool(tools, link.toolName);
  const args = linkArguments(tool, link.params);
  const query = new URLSearchParams(link.params).toString();
  const base = source.kind === 'local' ? `local:${source.serverId}` : `cloud:${source.instanceId}`;
  const key = `${base}/${tool.name}${query ? `?${query}` : ''}`;
  return {
    key,
    kind: source.kind,
    title: tool.title || tool.name,
    subtitle: source.subtitle,
    icons: source.icons,
    seed: source.kind === 'local' ? key : source.seed,
    toolName: tool.name,
    resourceUri,
    requiresInput: (tool.inputSchema?.required ?? []).length > 0,
    serverId: source.serverId,
    instanceId: source.instanceId,
    args,
  };
}
