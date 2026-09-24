/**
 * The workspace library (AgentArea "cloud" files) for plugins that declared the
 * "cloud" permission. Requests go through apiRequest — bearer and workspace are
 * added there — so a plugin never sees a token. A plugin's paths are relative
 * to its cloud root (`wiki/` by default) and can't leave it.
 */
import { apiRequest, getWorkspaceRef } from './api';
import { config } from './config';
import { getAccessToken } from './session';
import { cleanRel, encodePath, isText, listLevel, type CloudEntry, type Listing } from './cloudPaths';

export const SIGN_IN = 'Sign in to AgentArea to see cloud files';
const MAX_READ = 1 << 20;
/** One `/v1/files` listing serves a plugin's clicks for this long. */
const LISTING_TTL = 15_000;

let cache: { key: string; at: number; listing: Promise<Listing> } | null = null;

function requireSignIn() {
  if (!getAccessToken()) throw new Error(SIGN_IN);
}

async function listing(): Promise<Listing> {
  requireSignIn();
  const key = `${config.apiBaseUrl}|${getWorkspaceRef() ?? ''}`;
  if (!cache || cache.key !== key || Date.now() - cache.at > LISTING_TTL) {
    const next = apiRequest('/v1/files').then(async (res) => {
      if (res.status === 401) throw new Error(SIGN_IN);
      if (!res.ok) throw new Error(`AgentArea file list failed (${res.status})`);
      const raw = await res.json();
      return { files: Array.isArray(raw?.files) ? raw.files : [], directories: Array.isArray(raw?.directories) ? raw.directories : [] };
    });
    cache = { key, at: Date.now(), listing: next };
    // A failed list isn't cached.
    next.catch(() => {
      if (cache?.listing === next) cache = null;
    });
  }
  return cache.listing;
}

/** One folder level under `root`; entry paths are relative to `root`. */
export async function cloudList(root: string, prefix: string): Promise<CloudEntry[]> {
  const rel = cleanRel(prefix, true);
  const folder = (root + rel).replace(/\/+$/, '');
  return listLevel(await listing(), folder).map((e) => ({ ...e, path: e.path.slice(root.length) }));
}

/** A text file under `root`, at most 1 MiB. */
export async function cloudRead(root: string, path: string): Promise<string> {
  requireSignIn();
  const full = root + cleanRel(path, false);
  const res = await apiRequest(`/v1/files/download/${encodePath(full)}`);
  if (res.status === 404) throw new Error(`Not found in the workspace: ${full}`);
  if (res.status === 401) throw new Error(SIGN_IN);
  if (!res.ok) throw new Error(`Reading ${full} failed (${res.status})`);
  const type = res.headers.get('content-type');
  if (!isText(type, full)) throw new Error(`${full} is not a text file (${type ?? 'unknown type'})`);
  if (Number(res.headers.get('content-length') ?? 0) > MAX_READ) throw new Error(`${full} is larger than 1 MiB`);
  const bytes = new Uint8Array(await res.arrayBuffer());
  if (bytes.length > MAX_READ) throw new Error(`${full} is larger than 1 MiB`);
  if (bytes.includes(0)) throw new Error(`${full} looks binary`);
  try {
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } catch {
    throw new Error(`${full} is not UTF-8 text`);
  }
}

/** `agentarea://files/<full path>`: how a cloud file is referenced in a prompt. */
export function cloudRef(root: string, path: string): string {
  return `agentarea://files/${root}${cleanRel(path, false)}`;
}
