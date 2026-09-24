/**
 * Where local MCP servers come from: a bundled list of verified MCP Apps
 * examples, the public MCP registry, and registries of your own that speak
 * the same `/v0/servers` API (e.g. an AgentArea registry).
 *
 * A registry entry becomes a servers.json spec only when it ships a stdio
 * package we can launch: npm (`npx -y`) or PyPI (`uvx`). Remote-only servers
 * are shown but not installable yet.
 */
import { invoke } from '@tauri-apps/api/core';
import type { LocalServerSpec } from '@/lib/localMcp';

export const PUBLIC_REGISTRY = 'https://registry.modelcontextprotocol.io';

/** MCP Apps example servers from npm; all speak MCP over stdio with `--stdio`. */
const EXAMPLES: [pkg: string, name: string, description: string][] = [
  ['server-system-monitor', 'System monitor', 'Real-time CPU, memory and system stats'],
  ['server-budget-allocator', 'Budget allocator', 'Budget allocation with interactive visualization'],
  ['server-cohort-heatmap', 'Cohort heatmap', 'Cohort heatmap for retention analysis'],
  ['server-scenario-modeler', 'Scenario modeler', 'Financial scenario modeling'],
  ['server-customer-segmentation', 'Customer segmentation', 'Customer segmentation with filtering'],
  ['server-threejs', 'Three.js', 'Three.js 3D visualization'],
  ['server-map', 'Map', 'CesiumJS 3D globe and geocoding'],
  ['server-pdf', 'PDF viewer', 'Load PDFs, extract text with chunked pagination, interactive viewer'],
  ['server-wiki-explorer', 'Wiki explorer', 'Wikipedia link explorer with graph visualization'],
  ['server-sheet-music', 'Sheet music', 'Render and play sheet music from ABC notation'],
  ['server-shadertoy', 'Shadertoy', 'Render ShaderToy-compatible GLSL shaders'],
  ['server-basic-react', 'Basic React app', 'Basic MCP App example using React'],
];

export const RECOMMENDED: LocalServerSpec[] = EXAMPLES.map(([pkg, name, description]) => ({
  id: pkg.replace(/^server-/, ''),
  name,
  description,
  source: 'recommended',
  command: 'npx',
  args: ['-y', `@modelcontextprotocol/${pkg}`, '--stdio'],
}));

// ── registry API types (the parts we use) ────────────────────────────────────

interface RegistryArgument {
  type?: 'positional' | 'named';
  name?: string;
  value?: string;
  valueHint?: string;
  default?: string;
  isRequired?: boolean;
  description?: string;
  isSecret?: boolean;
}

interface RegistryEnvVar {
  name: string;
  description?: string;
  default?: string;
  isRequired?: boolean;
  isSecret?: boolean;
}

interface RegistryPackage {
  registryType?: string;
  identifier?: string;
  version?: string;
  transport?: { type?: string };
  runtimeArguments?: RegistryArgument[];
  packageArguments?: RegistryArgument[];
  environmentVariables?: RegistryEnvVar[];
}

export interface RegistryServer {
  name: string;
  title?: string;
  description?: string;
  version?: string;
  icons?: { src?: string }[];
  packages?: RegistryPackage[];
  remotes?: unknown[];
}

/** A value the user fills in before install: an env var or a required argument. */
export interface InstallInput {
  key: string;
  label: string;
  description?: string;
  required: boolean;
  secret: boolean;
  default?: string;
}

export type InstallPlan =
  | { kind: 'package'; runtime: 'npx' | 'uvx'; pkg: RegistryPackage; inputs: InstallInput[] }
  | { kind: 'remote' }
  | { kind: 'none' };

// ── search ───────────────────────────────────────────────────────────────────

function registryUrl(base: string, query: string): string {
  const q = new URLSearchParams({ limit: '30', version: 'latest' });
  if (query.trim()) q.set('search', query.trim());
  return `${base.replace(/\/+$/, '')}/v0/servers?${q}`;
}

/**
 * Search a registry. The public one is on the webview's http allowlist; any
 * other (user-typed) URL is fetched by Rust, https only.
 */
export async function searchRegistry(base: string, query: string): Promise<RegistryServer[]> {
  const url = registryUrl(base, query);
  let body: unknown;
  if (base.replace(/\/+$/, '') === PUBLIC_REGISTRY) {
    const { fetch } = await import('@tauri-apps/plugin-http');
    const res = await fetch(url, { headers: { Accept: 'application/json' } });
    if (!res.ok) throw new Error(`Registry search failed (${res.status})`);
    body = await res.json();
  } else {
    body = await invoke('http_get_json', { url });
  }
  const raw = (body as { servers?: unknown[] })?.servers;
  if (!Array.isArray(raw)) throw new Error('Not an MCP registry response');
  // Entries are `{ server, _meta }` (older registries: the server itself); one
  // name can appear per version, the last one wins.
  const byName = new Map<string, RegistryServer>();
  for (const entry of raw) {
    const server = ((entry as { server?: unknown })?.server ?? entry) as RegistryServer;
    if (server && typeof server.name === 'string') byName.set(server.name, server);
  }
  return [...byName.values()];
}

// ── registry entry → servers.json spec ───────────────────────────────────────

const STDIO = (p: RegistryPackage) => !p.transport?.type || p.transport.type === 'stdio';

function argInput(arg: RegistryArgument, i: number): InstallInput | null {
  if (arg.value !== undefined || !arg.isRequired) return null;
  return {
    key: `arg:${i}`,
    label: arg.name ?? arg.valueHint ?? `Argument ${i + 1}`,
    description: arg.description,
    required: true,
    secret: !!arg.isSecret,
    default: arg.default,
  };
}

export function installPlan(server: RegistryServer): InstallPlan {
  const pkg = (server.packages ?? []).find(
    (p) => STDIO(p) && p.identifier && (p.registryType === 'npm' || p.registryType === 'pypi'),
  );
  if (!pkg) return server.remotes?.length ? { kind: 'remote' } : { kind: 'none' };
  const inputs: InstallInput[] = [
    ...(pkg.environmentVariables ?? []).map((v) => ({
      key: `env:${v.name}`,
      label: v.name,
      description: v.description,
      required: !!v.isRequired,
      secret: !!v.isSecret,
      default: v.default,
    })),
    ...(pkg.packageArguments ?? []).flatMap((a, i) => argInput(a, i) ?? []),
  ];
  return { kind: 'package', runtime: pkg.registryType === 'npm' ? 'npx' : 'uvx', pkg, inputs };
}

/** `io.github.user/server` → `io.github.user-server`: fits the Rust id charset. */
export function slugId(name: string): string {
  return (
    name
      .toLowerCase()
      .replace(/[^a-z0-9._-]+/g, '-')
      .replace(/^[.-]+|-+$/g, '')
      .slice(0, 64) || 'server'
  );
}

/** Arguments with a fixed value (or a filled-in one); named ones become `--name value`. */
function argv(args: RegistryArgument[] | undefined, values: Record<string, string>, prefix: string | null): string[] {
  return (args ?? []).flatMap((a, i) => {
    const v = a.value ?? (prefix ? values[`${prefix}${i}`] : undefined) ?? (a.isRequired ? a.default : undefined);
    if (v === undefined || v === '') return [];
    if (a.type === 'named' && a.name) return [a.name.startsWith('-') ? a.name : `--${a.name}`, v];
    return [v];
  });
}

export function specFromRegistry(
  server: RegistryServer,
  plan: Extract<InstallPlan, { kind: 'package' }>,
  values: Record<string, string>,
  registry: string,
): LocalServerSpec {
  const { pkg, runtime } = plan;
  const ref = pkg.version ? `${pkg.identifier}@${pkg.version}` : pkg.identifier!;
  const runtimeArgs = argv(pkg.runtimeArguments, values, null);
  if (runtime === 'npx' && !runtimeArgs.includes('-y') && !runtimeArgs.includes('--yes')) runtimeArgs.unshift('-y');
  const env = Object.fromEntries(
    (pkg.environmentVariables ?? []).flatMap((v) => {
      const value = values[`env:${v.name}`] ?? v.default;
      return value ? [[v.name, value]] : [];
    }),
  );
  const icons = (server.icons ?? []).map((i) => i.src).filter((s): s is string => typeof s === 'string' && !!s);
  return {
    id: slugId(server.name),
    name: server.title || server.name,
    description: server.description,
    icons: icons.length ? icons : undefined,
    source: 'registry',
    registry,
    command: runtime,
    args: [...runtimeArgs, ref, ...argv(pkg.packageArguments, values, 'arg:')],
    env: Object.keys(env).length ? env : undefined,
  };
}

// ── saved custom registries ──────────────────────────────────────────────────

const REGISTRIES_KEY = 'aa.mcpRegistries';

export function savedRegistries(): string[] {
  try {
    const v = JSON.parse(localStorage.getItem(REGISTRIES_KEY) ?? '[]');
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
  } catch {
    return [];
  }
}

export function saveRegistries(list: string[]) {
  try {
    localStorage.setItem(REGISTRIES_KEY, JSON.stringify(list));
  } catch {
    /* best-effort */
  }
}

/** Split a command line on spaces, keeping "quoted parts" together. */
export function splitArgs(line: string): string[] {
  return [...line.matchAll(/"([^"]*)"|'([^']*)'|(\S+)/g)].map((m) => m[1] ?? m[2] ?? m[3]);
}
