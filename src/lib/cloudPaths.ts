/**
 * Pure helpers for AgentArea workspace files (`GET /v1/files` returns one flat
 * list): path checks and one-folder-level views. No imports on purpose, so the
 * same code runs in tests. The agent-side twin lives in src-tauri/src/data_mcp.rs.
 */

export interface WorkspaceFile {
  path: string;
  size: number;
  content_type?: string | null;
  last_modified?: string | null;
}

export interface Listing {
  files: WorkspaceFile[];
  /** trailing-slash folders, kept so empty ones show */
  directories: string[];
}

export interface CloudEntry {
  name: string;
  /** full workspace path; folders without the trailing "/" */
  path: string;
  dir: boolean;
}

/**
 * A path relative to a plugin's cloud root. No leading "/", `..`, `.`, empty
 * segments or backslashes, so it can't leave the root. `folder` allows a
 * trailing "/" and "" (the root itself).
 */
export function cleanRel(path: string, folder: boolean): string {
  let p = path.trim();
  if (p.startsWith('/')) throw new Error(`bad path: ${path} (paths are relative to the cloud folder)`);
  if (folder) p = p.replace(/\/+$/, '');
  if (!p) {
    if (folder) return '';
    throw new Error('path is required');
  }
  if (p.split('/').some((s) => !s || s === '.' || s === '..' || /[\\\0]/.test(s))) throw new Error(`bad path: ${path}`);
  return p;
}

/** A cloud root as stored: "" or "a/b/". */
export function cleanRoot(prefix: string): string {
  const p = prefix.trim().replace(/^\/+|\/+$/g, '');
  return p ? `${cleanRel(p, true)}/` : '';
}

/** Files directly in `folder` ("" = top, else no trailing "/") and the folders under it. */
export function listLevel(listing: Listing, folder: string): CloudEntry[] {
  const prefix = folder ? `${folder}/` : '';
  const out: CloudEntry[] = [];
  const dirs = new Set<string>();
  const addDir = (name: string) => {
    if (dirs.has(name)) return;
    dirs.add(name);
    out.push({ name, path: prefix + name, dir: true });
  };
  for (const f of listing.files) {
    if (!f.path.startsWith(prefix)) continue;
    const rest = f.path.slice(prefix.length);
    const slash = rest.indexOf('/');
    if (slash > 0) addDir(rest.slice(0, slash));
    else if (slash < 0 && rest) out.push({ name: rest, path: f.path, dir: false });
  }
  for (const d of listing.directories) {
    if (!d.startsWith(prefix)) continue;
    const name = d.slice(prefix.length).split('/')[0];
    if (name) addDir(name);
  }
  return out.sort((a, b) => Number(b.dir) - Number(a.dir) || a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
}

const TEXT_EXT = new Set(
  'md markdown mdx txt text json jsonl ndjson yaml yml toml csv tsv xml html htm css js mjs ts tsx jsx py rs go rb java sh sql ini cfg conf log rst org tex svg env'.split(
    ' ',
  ),
);

/** text/* and the usual structured types; for a generic or missing type, the extension decides. */
export function isText(contentType: string | null | undefined, path: string): boolean {
  const ct = (contentType ?? '').split(';')[0].trim().toLowerCase();
  if (ct.startsWith('text/')) return true;
  if (ct.startsWith('application/') && /json|xml|yaml|javascript|markdown|csv|toml|x-sh|sql|x-ndjson/.test(ct)) return true;
  if (ct && ct !== 'application/octet-stream' && ct !== 'binary/octet-stream') return false;
  const dot = path.lastIndexOf('.');
  return dot >= 0 && TEXT_EXT.has(path.slice(dot + 1).toLowerCase());
}

/** Each segment percent-encoded, "/" kept. */
export function encodePath(path: string): string {
  return path.split('/').map(encodeURIComponent).join('/');
}
