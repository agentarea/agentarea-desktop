import { createContext, useContext, useEffect, useState, type MouseEvent, type ReactNode } from 'react';
import {
  File,
  FileArchive,
  FileBraces,
  FileCode,
  FileImage,
  FileMusic,
  FileSpreadsheet,
  FileTerminal,
  FileText,
  FileVideoCamera,
  Folder,
  type LucideIcon,
} from 'lucide-react';
import { cn } from '@/lib/utils';
import type { Session } from '@/data';

/** The folder the thread's agent works in: relative file links resolve against it. */
const FolderContext = createContext<string | null>(null);

let home: string | null = null;
let homeLoad: Promise<void> | null = null;
const sessionDirs = new Map<string, string>();

function loadHome() {
  homeLoad ??= import('@tauri-apps/api/path')
    .then((m) => m.homeDir())
    .then((h) => void (home = h.replace(/\/$/, '')))
    .catch(() => {});
  return homeLoad;
}

/** Provides the session's working folder: its picked `cwd`, else the thread's own ~/AgentArea/<id>. */
export function ThreadFolder({ session, children }: { session: Session; children: ReactNode }) {
  const [dir, setDir] = useState<string | null>(session.cwd ?? sessionDirs.get(session.id) ?? null);
  useEffect(() => {
    void loadHome();
    if (session.runner === 'cloud') return setDir(null);
    if (session.cwd) return setDir(session.cwd);
    const known = sessionDirs.get(session.id);
    if (known) return setDir(known);
    let live = true;
    import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke<string>('session_dir', { sessionId: session.id }))
      .then((d) => {
        sessionDirs.set(session.id, d);
        if (live) setDir(d);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [session.id, session.cwd, session.runner]);
  return <FolderContext.Provider value={dir}>{children}</FolderContext.Provider>;
}

export const useThreadFolder = () => useContext(FolderContext);

export interface LocalRef {
  path: string;
  line?: string;
}

function decode(s: string) {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

/** Drop `.` and `..` segments. */
function normalize(path: string) {
  const out: string[] = [];
  for (const seg of path.split('/')) {
    if (seg === '..') out.pop();
    else if (seg !== '.' && seg !== '') out.push(seg);
  }
  return '/' + out.join('/') + (path.endsWith('/') && out.length ? '/' : '');
}

/**
 * A link target that names a local file: `/abs`, `~/…`, `file://…`, or a path
 * relative to the thread folder, with an optional `#L12` / `:12` line. Null
 * for web links, anchors and anything with another scheme.
 */
export function localRef(href: string, folder: string | null): LocalRef | null {
  let h = href.trim();
  if (!h || h.startsWith('#')) return null;
  if (/^file:/i.test(h)) h = h.replace(/^file:\/\/(localhost)?/i, '');
  else if (/^[a-z][a-z0-9+.-]*:/i.test(h)) return null;
  h = decode(h);
  let line: string | undefined;
  const hash = /#L(\d+)(?:-L?(\d+))?$/.exec(h);
  const colon = /:(\d+)(?::\d+)?$/.exec(h);
  if (hash) {
    line = hash[2] ? `${hash[1]}-${hash[2]}` : hash[1];
    h = h.slice(0, hash.index);
  } else if (colon) {
    line = colon[1];
    h = h.slice(0, colon.index);
  }
  if (h.startsWith('~/') || h === '~') {
    if (!home) return null;
    h = home + h.slice(1);
  } else if (!h.startsWith('/')) {
    if (!folder) return null;
    h = folder.replace(/\/$/, '') + '/' + h;
  }
  return { path: normalize(h), line };
}

export const basename = (path: string) => path.replace(/\/$/, '').split('/').pop() || path;

const EXT_ICONS: [RegExp, LucideIcon][] = [
  [/\.(png|jpe?g|gif|webp|svg|ico|bmp|heic|avif|tiff?)$/i, FileImage],
  [/\.(json|jsonc|json5|ya?ml|toml|lock)$/i, FileBraces],
  [/\.(csv|tsv|xlsx?|numbers|parquet)$/i, FileSpreadsheet],
  [/\.(zip|tar|gz|tgz|bz2|xz|7z|rar|dmg)$/i, FileArchive],
  [/\.(mp4|mov|webm|mkv|avi)$/i, FileVideoCamera],
  [/\.(mp3|wav|m4a|flac|ogg|aac)$/i, FileMusic],
  [/\.(sh|bash|zsh|fish|ps1|bat)$/i, FileTerminal],
  [
    /\.([cm]?[jt]sx?|py|rs|go|rb|java|kt|swift|c|cc|cpp|h|hpp|cs|php|lua|sql|html?|css|scss|vue|svelte|ipynb|dart|scala|ex|exs|zig)$/i,
    FileCode,
  ],
  [/\.(md|mdx|txt|rtf|pdf|docx?|pages|log|rst|tex)$/i, FileText],
];

export function fileIcon(path: string): LucideIcon {
  if (path.endsWith('/')) return Folder;
  return EXT_ICONS.find(([re]) => re.test(path))?.[1] ?? File;
}

/** Open a local file with its default app; ⌘ (or `reveal`) shows it in Finder instead. */
export async function openLocal(path: string, reveal = false) {
  try {
    const opener = await import('@tauri-apps/plugin-opener');
    if (reveal) await opener.revealItemInDir(path);
    else await opener.openPath(path).catch(() => opener.revealItemInDir(path));
  } catch {
    // not in the Tauri window (browser dev) or the file is gone
  }
}

export async function openExternal(url: string) {
  try {
    const { openUrl } = await import('@tauri-apps/plugin-opener');
    await openUrl(url);
  } catch {
    window.open(url, '_blank', 'noopener');
  }
}

/** A local file reference, Codex style: icon + name, click to open, ⌘-click to reveal. */
export function FileChip({ file, label, className }: { file: LocalRef; label?: ReactNode; className?: string }) {
  const Icon = fileIcon(file.path);
  const click = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    void openLocal(file.path, e.metaKey);
  };
  return (
    <a
      href={`file://${file.path}`}
      onClick={click}
      title={`${file.path}${file.line ? `:${file.line}` : ''}\nClick to open · ⌘-click to reveal in Finder`}
      className={cn(
        'inline-flex max-w-full items-baseline gap-1 rounded-md bg-muted px-1.5 py-px align-baseline text-[0.92em] font-medium text-foreground no-underline transition-colors hover:bg-accent',
        className,
      )}
    >
      <Icon className="size-[1em] shrink-0 translate-y-[0.14em] text-muted-foreground" />
      <span className="truncate">
        {label ?? basename(file.path)}
        {file.line && <span className="font-normal text-muted-foreground">:{file.line}</span>}
      </span>
    </a>
  );
}
