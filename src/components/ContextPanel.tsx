import { useEffect, useMemo, useState } from 'react';
import {
  Bot,
  ChevronDown,
  Cloud,
  Code2,
  FileText,
  Folder,
  FolderOpen,
  Laptop,
  Library,
  Paperclip,
  PanelRight,
  Pencil,
  Plug,
  Plus,
  Sparkles,
  Webhook,
  Wrench,
  X,
} from 'lucide-react';
import { create } from 'zustand';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';
import { apiRequest } from '@/lib/api';
import { usePlugins } from '@/lib/plugins';
import { pluginSection, useSidebarLayout } from '@/lib/sidebarLayout';
import { RUNNER_LABEL } from '@/assets/icons';
import type { Message, Session } from '@/data';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { basename } from './format';
import { FileChip } from './FileChip';
import { FolderChip } from './Composer';
import { EntityIcon } from './EntityIcon';
import { PluginFrame } from './PluginFrame';

/* ── Context panel: everything a thread's agent works with, on the right ── */

const OPEN_KEY = 'aa.contextPanel';

export const useContextPanel = create<{ open: boolean; toggle: () => void; close: () => void }>((set, get) => ({
  open: (() => {
    try {
      return localStorage.getItem(OPEN_KEY) === '1';
    } catch {
      return false;
    }
  })(),
  toggle: () => {
    const open = !get().open;
    try {
      localStorage.setItem(OPEN_KEY, open ? '1' : '0');
    } catch {
      /* private storage */
    }
    set({ open });
  },
  close: () => {
    try {
      localStorage.setItem(OPEN_KEY, '0');
    } catch {
      /* private storage */
    }
    set({ open: false });
  },
}));

/* ── a cloud agent's tools (GET /v1/agents/{id}) ── */

export interface AgentTool {
  type: 'code' | 'mcp' | 'agent' | 'openapi' | string;
  name: string;
  /** mcp / openapi: the allowed subset; empty = all of the server's tools */
  allowed: string[];
  description?: string;
}
export interface AgentDetail {
  tools: AgentTool[];
  /** the tool configs as the API returned them, sent back unchanged on PATCH */
  raw: unknown[];
  skills: { id: string; name: string; description?: string }[];
  model?: string | null;
}

const agentCache = new Map<string, Promise<AgentDetail>>();

async function fetchAgent(id: string): Promise<AgentDetail> {
  const res = await apiRequest(`/v1/agents/${encodeURIComponent(id)}`);
  if (!res.ok) throw new Error(`GET /v1/agents/${id} failed (${res.status})`);
  const a = await res.json();
  const tools: AgentTool[] = (Array.isArray(a.tools) ? a.tools : []).map((t: any) => ({
    type: String(t.type ?? 'code'),
    name: String(t.name ?? ''),
    allowed: (t.settings?.allowed_tools ?? []).map((x: any) => (typeof x === 'string' ? x : x?.tool_name)).filter(Boolean),
    description: t.settings?.description_override ?? undefined,
  }));
  return { tools, raw: Array.isArray(a.tools) ? a.tools : [], skills: Array.isArray(a.skills) ? a.skills : [], model: a.model_id ?? null };
}

const agentListeners = new Set<(id: string) => void>();

/**
 * Add or remove one MCP server / skill on a cloud agent (PATCH /v1/agents/{id}).
 * It changes the agent itself, so every later task of that agent gets it too.
 */
export async function patchAgentMember(agentId: string, kind: 'mcp' | 'skill', id: string, add: boolean): Promise<void> {
  const cur = await (agentCache.get(agentId) ?? fetchAgent(agentId));
  const body =
    kind === 'mcp'
      ? {
          tools: add
            ? [...cur.raw, { type: 'mcp', name: id, settings: { allowed_tools: null } }]
            : cur.raw.filter((t: any) => !(t?.type === 'mcp' && t?.name === id)),
        }
      : { skill_ids: add ? [...cur.skills.map((x) => x.id), id] : cur.skills.map((x) => x.id).filter((x) => x !== id) };
  const res = await apiRequest(`/v1/agents/${encodeURIComponent(agentId)}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`Couldn't update the agent (${res.status}): ${(await res.text()).slice(0, 200)}`);
  agentCache.set(agentId, fetchAgent(agentId));
  agentListeners.forEach((f) => f(agentId));
}

/** The cloud agent's tools and skills; null while loading, `error` when the API refused. */
export function useAgentDetail(agentId: string | null | undefined) {
  const [state, setState] = useState<{ detail: AgentDetail | null; error: string | null }>({ detail: null, error: null });
  const [rev, setRev] = useState(0);
  useEffect(() => {
    const f = (id: string) => id === agentId && setRev((r) => r + 1);
    agentListeners.add(f);
    return () => void agentListeners.delete(f);
  }, [agentId]);
  useEffect(() => {
    if (!agentId) return setState({ detail: null, error: null });
    let live = true;
    if (!agentCache.has(agentId)) agentCache.set(agentId, fetchAgent(agentId));
    agentCache
      .get(agentId)!
      .then((detail) => live && setState({ detail, error: null }))
      .catch((e) => {
        agentCache.delete(agentId);
        if (live) setState({ detail: null, error: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      live = false;
    };
  }, [agentId, rev]);
  return state;
}

const TOOL_KIND: Record<string, { label: string; icon: typeof Wrench }> = {
  code: { label: 'Built-in', icon: Code2 },
  mcp: { label: 'MCP server', icon: Plug },
  agent: { label: 'Agent', icon: Bot },
  openapi: { label: 'API', icon: Webhook },
};

/** MCP instance / agent ids shown by name when the workspace lists them. */
function useToolName() {
  const mcps = useAppStore((s) => s.mcps);
  const agents = useAppStore((s) => s.agents);
  return (t: AgentTool) =>
    (t.type === 'mcp' ? mcps.find((m) => m.id === t.name)?.name : t.type === 'agent' ? agents.find((a) => a.id === t.name)?.name : null) ??
    t.name;
}

function ToolRows({ detail }: { detail: AgentDetail }) {
  const nameOf = useToolName();
  return (
    <>
      {detail.tools.map((t) => {
        const kind = TOOL_KIND[t.type] ?? { label: t.type, icon: Wrench };
        return (
          <div key={t.type + t.name} className="flex items-start gap-2 px-2 py-1.5 text-[13px]">
            <kind.icon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
            <div className="min-w-0">
              <div className="truncate">{nameOf(t)}</div>
              <div className="text-[11px] text-muted-foreground">
                {kind.label}
                {t.allowed.length ? ` · ${t.allowed.length} tool${t.allowed.length > 1 ? 's' : ''}: ${t.allowed.join(', ')}` : t.type === 'mcp' ? ' · all tools' : ''}
                {t.description ? ` · ${t.description}` : ''}
              </div>
            </div>
          </div>
        );
      })}
      {detail.skills.map((s) => (
        <div key={s.id} className="flex items-start gap-2 px-2 py-1.5 text-[13px]" title={s.description}>
          <Sparkles className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
          <div className="min-w-0">
            <div className="truncate">{s.name}</div>
            <div className="truncate text-[11px] text-muted-foreground">Skill{s.description ? ` · ${s.description}` : ''}</div>
          </div>
        </div>
      ))}
    </>
  );
}

/** Composer chip for a cloud thread: "Tools · N" with the agent's tools in a dropdown. */
export function AgentToolsChip({ session }: { session: Session }) {
  const { detail, error } = useAgentDetail(session.runner === 'cloud' ? session.agent?.id : null);
  if (session.runner !== 'cloud') return null;
  const n = detail ? detail.tools.length + detail.skills.length : null;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button className="flex items-center gap-1.5 rounded-full px-2 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground">
          <Wrench className="size-3.5" />
          {error ? 'Tools' : n === null ? 'Tools…' : n ? `Tools · ${n}` : 'No tools'}
          <ChevronDown className="size-3 opacity-60" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-[60vh] w-80 overflow-y-auto">
        <DropdownMenuLabel>{session.agent?.name ?? 'Agent'} · its own tools</DropdownMenuLabel>
        {error ? (
          <div className="px-2 py-1.5 text-[12px] text-muted-foreground">Couldn't load the agent: {error}</div>
        ) : !detail ? (
          <div className="px-2 py-1.5 text-[12px] text-muted-foreground">Loading…</div>
        ) : n ? (
          <ToolRows detail={detail} />
        ) : (
          <div className="px-2 py-1.5 text-[12px] text-muted-foreground">This agent has no tools — it only answers.</div>
        )}
        <DropdownMenuSeparator />
        <div className="px-2 py-1.5 text-[11px] text-muted-foreground">Edit them in AgentArea → Agents.</div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/* ── files the thread touched (from its tool calls) ── */

const WRITE_TOOLS = new Set(['Write', 'Edit', 'MultiEdit', 'NotebookEdit', 'fileChange']);
const READ_TOOLS = new Set(['Read', 'NotebookRead']);

function pathsOf(m: Message): string[] {
  let input: any;
  try {
    input = JSON.parse(m.text);
  } catch {
    return [];
  }
  const out: string[] = [];
  for (const k of ['file_path', 'notebook_path', 'path']) if (typeof input?.[k] === 'string') out.push(input[k]);
  if (Array.isArray(input?.changes)) for (const c of input.changes) if (typeof c?.path === 'string') out.push(c.path);
  return out;
}

function threadFiles(messages: Message[]) {
  const changed = new Map<string, number>();
  const read = new Map<string, number>();
  for (const m of messages) {
    if (m.role !== 'tool' || !m.tool) continue;
    const into = WRITE_TOOLS.has(m.tool) ? changed : READ_TOOLS.has(m.tool) ? read : null;
    if (!into) continue;
    for (const p of pathsOf(m)) into.set(p, (into.get(p) ?? 0) + 1);
  }
  for (const p of changed.keys()) read.delete(p);
  return { changed: [...changed.keys()], read: [...read.keys()] };
}

/* ── local data sources (same Rust list the agent gets) ── */

interface DataSource {
  name: string;
  kind: 'local' | 'cloud';
  location: string;
  description: string;
}

function useDataSources(session: Session) {
  const plugins = usePlugins((s) => s.plugins);
  const [local, setLocal] = useState<DataSource[]>([]);
  useEffect(() => {
    if (session.runner === 'cloud') return setLocal([]);
    let live = true;
    void import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke<DataSource[]>('data_sources', { sessionId: session.id, cwd: session.cwd }))
      .then((list) => live && setLocal(list))
      .catch(() => live && setLocal([]));
    return () => {
      live = false;
    };
  }, [session.id, session.cwd, session.runner, plugins]);
  return local;
}

let home: Promise<string> | null = null;
function useTilde() {
  const [dir, setDir] = useState('');
  useEffect(() => {
    home ??= import('@tauri-apps/api/path').then((m) => m.homeDir()).catch(() => '');
    void home.then(setDir);
  }, []);
  return (p: string) => (dir && p.startsWith(dir) ? `~${p.slice(dir.replace(/\/+$/, '').length)}` : p);
}

/* ── the panel ── */

function Section({ title, count, children }: { title: string; count?: number; children: React.ReactNode }) {
  return (
    <section className="border-b border-border px-3 py-3 last:border-b-0">
      <h3 className="mb-1.5 flex items-center gap-1.5 px-2 text-[11px] font-semibold tracking-wide text-muted-foreground uppercase">
        {title}
        {count ? <span className="font-normal text-muted-foreground-subtle">{count}</span> : null}
      </h3>
      {children}
    </section>
  );
}

function Row({ icon: Icon, title, detail, onClick }: { icon: typeof Cloud; title: string; detail?: string; onClick?: () => void }) {
  return (
    <div
      className={cn('flex items-start gap-2 rounded-md px-2 py-1.5 text-[13px]', onClick && 'cursor-pointer hover:bg-accent')}
      title={detail}
      onClick={onClick}
    >
      <Icon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
      <div className="min-w-0">
        <div className="truncate">{title}</div>
        {detail && <div className="truncate text-[11px] text-muted-foreground">{detail}</div>}
      </div>
    </div>
  );
}

const Empty = ({ children }: { children: React.ReactNode }) => (
  <div className="px-2 py-1 text-[12px] text-muted-foreground">{children}</div>
);

/**
 * What the agent will use, editable in place: a local thread's MCP servers and
 * skills (this thread only), or a cloud agent's own (changes the agent).
 */
function ToolsSection({ session }: { session: Session }) {
  const mcps = useAppStore((s) => s.mcps);
  const skills = useAppStore((s) => s.skills);
  const toggleMember = useAppStore((s) => s.toggleMember);
  const connected = useAppStore((s) => s.connection === 'connected');
  const cloud = session.runner === 'cloud';
  const agentId = cloud ? session.agent?.id ?? null : null;
  const { detail, error } = useAgentDetail(agentId);
  const nameOf = useToolName();
  const [busy, setBusy] = useState<string | null>(null);
  const [fail, setFail] = useState<string | null>(null);

  const usedMcp = cloud ? (detail?.tools.filter((t) => t.type === 'mcp').map((t) => t.name) ?? []) : session.mcpIds;
  const usedSkill = cloud ? (detail?.skills.map((x) => x.id) ?? []) : session.skillIds;
  const change = async (kind: 'mcp' | 'skill', id: string, add: boolean) => {
    setBusy(id);
    setFail(null);
    try {
      if (cloud && agentId) await patchAgentMember(agentId, kind, id, add);
      else await toggleMember(session.id, kind, id);
    } catch (e) {
      setFail(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };
  const freeMcps = mcps.filter((m) => !usedMcp.includes(m.id));
  const freeSkills = skills.filter((x) => !usedSkill.includes(x.id));
  const loading = cloud && !detail && !error;
  const count = cloud ? (detail ? detail.tools.length + detail.skills.length : undefined) : usedMcp.length + usedSkill.length;

  const remove = (kind: 'mcp' | 'skill', id: string) => (
    <button
      onClick={() => void change(kind, id, false)}
      disabled={!connected || busy === id}
      aria-label="Remove"
      title={cloud ? 'Remove from the agent' : 'Remove from this thread'}
      className="rounded p-0.5 text-muted-foreground opacity-0 group-hover:opacity-100 hover:text-foreground disabled:opacity-30"
    >
      <X className="size-3" />
    </button>
  );

  return (
    <Section title={cloud ? `${session.agent?.name ?? 'Agent'} uses` : 'Tools'} count={count}>
      {error && <Empty>Couldn't load the agent: {error}</Empty>}
      {loading && <Empty>Loading the agent's tools…</Empty>}
      {cloud &&
        detail?.tools
          .filter((t) => t.type !== 'mcp')
          .map((t) => {
            const kind = TOOL_KIND[t.type] ?? { label: t.type, icon: Wrench };
            return (
              <Row
                key={t.type + t.name}
                icon={kind.icon}
                title={nameOf(t)}
                detail={`${kind.label}${t.allowed.length ? ` · ${t.allowed.join(', ')}` : ''}${t.description ? ` · ${t.description}` : ''}`}
              />
            );
          })}
      {usedMcp.map((id) => {
        const t = detail?.tools.find((x) => x.type === 'mcp' && x.name === id);
        const m = mcps.find((x) => x.id === id);
        return (
          <div key={id} className="group flex items-start gap-2 rounded-md px-2 py-1.5 text-[13px] hover:bg-accent">
            {m ? <EntityIcon name={m.name} icons={m.icons} seed={m.id} className="mt-0.5 size-3.5" /> : <Plug className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />}
            <div className="min-w-0 flex-1">
              <div className="truncate">{m?.name ?? id}</div>
              <div className="truncate text-[11px] text-muted-foreground">
                MCP server{t ? (t.allowed.length ? ` · ${t.allowed.join(', ')}` : ' · all tools') : ''}
              </div>
            </div>
            {remove('mcp', id)}
          </div>
        );
      })}
      {usedSkill.map((id) => {
        const k = skills.find((x) => x.id === id) ?? detail?.skills.find((x) => x.id === id);
        return (
          <div key={id} className="group flex items-start gap-2 rounded-md px-2 py-1.5 text-[13px] hover:bg-accent" title={k?.description}>
            <Sparkles className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="truncate">{k?.name ?? id}</div>
              <div className="truncate text-[11px] text-muted-foreground">Skill{k?.description ? ` · ${k.description}` : ''}</div>
            </div>
            {remove('skill', id)}
          </div>
        );
      })}
      {!loading && !error && count === 0 && <Empty>{cloud ? 'This agent has no tools yet.' : 'No MCP servers or skills yet.'}</Empty>}
      {fail && <div className="px-2 py-1 text-[11.5px] text-destructive">{fail}</div>}
      {!error && (
        <DropdownMenu>
          <DropdownMenuTrigger asChild disabled={!connected || loading}>
            <button className="mt-1 ml-1 flex items-center gap-1.5 rounded-md px-1.5 py-1 text-[12px] text-muted-foreground hover:bg-accent hover:text-foreground disabled:opacity-50">
              <Plus className="size-3.5" /> Add MCP or skill
            </button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="start" className="max-h-96 w-72 overflow-y-auto">
            <DropdownMenuLabel>{cloud ? `Adds to ${session.agent?.name ?? 'the agent'} — all its tasks` : 'Adds to this thread'}</DropdownMenuLabel>
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-[11px] text-muted-foreground">MCP servers</DropdownMenuLabel>
            {freeMcps.length === 0 && <div className="px-2 py-1 text-[12px] text-muted-foreground">All added</div>}
            {freeMcps.map((m) => (
              <DropdownMenuItem key={m.id} onSelect={() => void change('mcp', m.id, true)}>
                <EntityIcon name={m.name} icons={m.icons} seed={m.id} />
                <span className="truncate">{m.name}</span>
              </DropdownMenuItem>
            ))}
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-[11px] text-muted-foreground">Skills</DropdownMenuLabel>
            {freeSkills.length === 0 && <div className="px-2 py-1 text-[12px] text-muted-foreground">All added</div>}
            {freeSkills.map((k) => (
              <DropdownMenuItem key={k.id} onSelect={() => void change('skill', k.id, true)} title={k.description}>
                <Sparkles />
                <span className="truncate">{k.name}</span>
                {k.builtin && <span className="ml-auto text-[10.5px] text-muted-foreground">built-in</span>}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
      )}
      {!connected && <Empty>Connect to your workspace to change tools.</Empty>}
    </Section>
  );
}

const CONTEXT_PLUGIN = 'llm-wiki';
const TAB_KEY = 'aa.contextPanel.tab';
const MOVED_KEY = 'aa.contextPanel.treeMoved';

function TabButton({ on, onClick, children }: { on: boolean; onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      onClick={onClick}
      className={cn(
        'h-7 rounded-md px-2.5 text-[12.5px] font-medium transition-colors',
        on ? 'bg-accent text-foreground' : 'text-muted-foreground hover:text-foreground',
      )}
    >
      {children}
    </button>
  );
}

/** Toggle for the panel: top-right of a thread. */
export function ContextToggle({
  session,
  className,
  open: openProp,
  onToggle,
}: {
  session: Session;
  className?: string;
  /** a pane keeps its own open state; the main view uses the shared one */
  open?: boolean;
  onToggle?: () => void;
}) {
  const shared = useContextPanel((s) => s.open);
  const sharedToggle = useContextPanel((s) => s.toggle);
  const open = openProp ?? shared;
  const toggle = onToggle ?? sharedToggle;
  const files = useMemo(() => threadFiles(session.messages), [session.messages]);
  const n = files.changed.length + session.attachments.length;
  return (
    <button
      onClick={toggle}
      aria-pressed={open}
      title={open ? 'Hide context' : 'Context: tools, folder, data and files of this thread'}
      className={cn(
        'flex h-7 items-center gap-1.5 rounded-md px-2 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground',
        open && 'bg-accent text-foreground',
        className,
      )}
    >
      <PanelRight className="size-3.5" />
      <span>Context</span>
      {n > 0 && <span className="rounded-full bg-foreground/10 px-1.5 text-[10.5px] font-medium tabular-nums">{n}</span>}
    </button>
  );
}

export function ContextPanel({ session, className, onClose }: { session: Session; className?: string; onClose?: () => void }) {
  const sharedClose = useContextPanel((s) => s.close);
  const close = onClose ?? sharedClose;
  const removeAttachment = useAppStore((s) => s.removeAttachment);
  const signedIn = useAppStore((s) => s.auth.status === 'authenticated');
  const workspace = useAppStore((s) => s.workspaces.find((w) => w.id === s.workspaceId)?.name ?? null);
  const plugins = usePlugins((s) => s.plugins);
  const cloud = session.runner === 'cloud';
  const sources = useDataSources(session);
  const tilde = useTilde();
  const files = useMemo(() => threadFiles(session.messages), [session.messages]);
  // The Context plugin's file tree (its sidebar page) is the panel's Files tab.
  const tree = plugins.find((p) => p.id === CONTEXT_PLUGIN && p.enabled && p.sidebar);
  const [tab, setTab] = useState<'files' | 'agent'>(() => {
    try {
      return localStorage.getItem(TAB_KEY) === 'files' ? 'files' : 'agent';
    } catch {
      return 'agent';
    }
  });
  const pick = (t: 'files' | 'agent') => {
    setTab(t);
    try {
      localStorage.setItem(TAB_KEY, t);
    } catch {
      /* private storage */
    }
  };
  const showFiles = tab === 'files' && !!tree;
  // The tree moved here from the left sidebar: take it out of there once (the Apps screen brings it back).
  useEffect(() => {
    if (!tree) return;
    try {
      if (localStorage.getItem(MOVED_KEY)) return;
      localStorage.setItem(MOVED_KEY, '1');
    } catch {
      return;
    }
    useSidebarLayout.getState().setHidden(pluginSection(tree.id), true);
  }, [tree]);
  const hints = plugins.filter((p) => p.enabled && p.permissions.includes('cloud') && p.cloudRoot !== null);
  const reveal = (path: string) =>
    void import('@tauri-apps/plugin-opener').then((o) => o.revealItemInDir(path)).catch(() => {});

  return (
    <aside className={cn('flex w-[300px] shrink-0 flex-col border-l border-border bg-background', className)}>
      <div className="flex h-10 shrink-0 items-center gap-1 border-b border-border pr-1.5 pl-2">
        {tree && (
          <TabButton on={showFiles} onClick={() => pick('files')}>
            Files
          </TabButton>
        )}
        <TabButton on={!showFiles} onClick={() => pick('agent')}>
          Agent
        </TabButton>
        <span className="ml-1 truncate text-[11.5px] text-muted-foreground">
          {cloud ? `Cloud · ${session.agent?.name ?? 'agent'}` : `Local · ${RUNNER_LABEL[session.runner]}`}
        </span>
        <span className="flex-1" />
        <button onClick={close} aria-label="Close context" className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground">
          <X className="size-3.5" />
        </button>
      </div>
      {showFiles && tree && <PluginFrame plugin={tree} page={tree.sidebar ?? undefined} className="min-h-0 w-full flex-1 border-0 bg-background" />}
      <div className={cn('min-h-0 flex-1 overflow-y-auto scrollbar-sleek', showFiles && 'hidden')}>
        <ToolsSection session={session} />

        {!cloud && (
          <Section title="Folder">
            <div className="flex items-center gap-2 px-2 py-1">
              <FolderChip session={session} />
              {session.cwd && (
                <button
                  onClick={() => reveal(session.cwd!)}
                  className="flex items-center gap-1 text-[11.5px] text-muted-foreground hover:text-foreground"
                  title="Reveal in Finder"
                >
                  <FolderOpen className="size-3.5" /> Reveal
                </button>
              )}
            </div>
            {session.cwd && <div className="truncate px-2 text-[11px] text-muted-foreground">{tilde(session.cwd)}</div>}
          </Section>
        )}

        {!cloud && (
          <Section title="Data the agent reads" count={sources.length + (signedIn ? 1 : 0)}>
            {sources.map((s) => (
              <Row key={s.location + s.name} icon={s.name === 'Thread folder' ? Folder : Laptop} title={s.name} detail={tilde(s.location)} onClick={() => reveal(s.location)} />
            ))}
            {signedIn ? (
              <Row
                icon={Cloud}
                title={`${workspace ?? 'Workspace'} files`}
                detail={hints.length ? hints.map((p) => `${p.title}: ${p.cloudRoot || '/'}`).join(' · ') : 'The workspace library, via agentarea_data'}
              />
            ) : (
              <Empty>Sign in to include cloud files.</Empty>
            )}
          </Section>
        )}

        <Section title="Attached" count={session.attachments.length}>
          {session.attachments.length ? (
            session.attachments.map((p) => (
              <div key={p} className="group flex items-center gap-2 rounded-md px-2 py-1.5 text-[13px] hover:bg-accent">
                {p.startsWith('agentarea://files/') ? <Cloud className="size-3.5 shrink-0 text-muted-foreground" /> : <Paperclip className="size-3.5 shrink-0 text-muted-foreground" />}
                <span className="min-w-0 flex-1 truncate" title={p}>
                  {basename(p)}
                </span>
                <button
                  onClick={() => removeAttachment(session.id, p)}
                  aria-label={`Remove ${basename(p)}`}
                  className="rounded p-0.5 text-muted-foreground opacity-0 group-hover:opacity-100 hover:text-foreground"
                >
                  <X className="size-3" />
                </button>
              </div>
            ))
          ) : (
            <Empty>{cloud ? 'Cloud threads take no attachments.' : 'Drop files on the thread to attach them to the next message.'}</Empty>
          )}
        </Section>

        <Section title="Files in this thread" count={files.changed.length + files.read.length}>
          {files.changed.length + files.read.length === 0 && <Empty>The agent hasn't read or changed files yet.</Empty>}
          {files.changed.map((p) => (
            <div key={p} className="flex items-center gap-2 px-2 py-1 text-[12.5px]">
              <Pencil className="size-3.5 shrink-0 text-chart-2" aria-label="Changed" />
              <FileChip file={{ path: p }} label={basename(p)} className="min-w-0 truncate" />
            </div>
          ))}
          {files.read.map((p) => (
            <div key={p} className="flex items-center gap-2 px-2 py-1 text-[12.5px]">
              <FileText className="size-3.5 shrink-0 text-muted-foreground" aria-label="Read" />
              <FileChip file={{ path: p }} label={basename(p)} className="min-w-0 truncate" />
            </div>
          ))}
        </Section>
        <div className="px-5 py-3 text-[11px] text-muted-foreground-subtle">
          <Library className="mr-1 inline size-3 align-[-2px]" />
          The agent sees tools, folder and data on every turn.
        </div>
      </div>
    </aside>
  );
}
