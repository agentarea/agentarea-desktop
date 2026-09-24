import { useEffect, useState } from 'react';
import {
  Plug,
  Sparkles,
  ArrowUp,
  Square,
  ChevronDown,
  Cloud,
  Folder,
  FolderOpen,
  RotateCcw,
  Paperclip,
  X,
  Brain,
  Clock,
  Library,
  Laptop,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { useAppStore, type MemberKind } from '@/store';
import { RunnerBadge, RUNNER_LABEL, ClaudeIcon, CodexIcon } from '@/assets/icons';
import { basename } from './format';
import type { CatalogItem, Session } from '@/data';
import { modelsFor } from '@/lib/models';
import { usePlugins } from '@/lib/plugins';
import { EntityIcon } from './EntityIcon';

/* ── Composer: prompt + attachments + tools + where it runs ─ */

export function Composer({ session, autoFocus }: { session: Session; autoFocus?: boolean }) {
  const mcps = useAppStore((s) => s.mcps);
  const skills = useAppStore((s) => s.skills);
  const running = useAppStore((s) => !!s.running[session.id]);
  const send = useAppStore((s) => s.send);
  const stop = useAppStore((s) => s.stop);
  const [draft, setDraft] = useState('');
  // A local turn can be interrupted; a cloud one is managed in AgentArea.
  const canStop = running && session.runner !== 'cloud';

  const submit = () => {
    const text = draft.trim();
    if (!text || running) return;
    setDraft('');
    void send(session.id, text);
  };

  return (
    <div>
      <AttachmentChips session={session} />
      <div className="rounded-3xl border border-border bg-background p-2.5 shadow-[0_2px_12px_rgba(0,0,0,0.06)] transition-shadow focus-within:shadow-[0_2px_16px_rgba(0,0,0,0.09)]">
        <textarea
          value={draft}
          autoFocus={autoFocus}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.nativeEvent.isComposing) {
              e.preventDefault();
              submit();
            }
          }}
          rows={autoFocus ? 3 : 1}
          placeholder={session.runner === 'cloud' ? 'Describe a task for the agent…' : 'Ask anything…'}
          className="max-h-48 min-h-9 w-full resize-none bg-transparent px-2.5 py-2 text-[14.5px] outline-none field-sizing-content placeholder:text-muted-foreground"
        />
        <div className="flex items-center gap-1 px-0.5 pt-1">
          {session.runner === 'cloud' ? (
            <span className="px-1.5 text-[12px] text-muted-foreground">Uses the agent's own tools</span>
          ) : (
            <>
              <MemberPicker session={session} kind="mcp" label="MCP" icon={Plug} items={mcps} selected={session.mcpIds} />
              <MemberPicker
                session={session}
                kind="skill"
                label="Skills"
                icon={Sparkles}
                items={skills}
                selected={session.skillIds}
              />
            </>
          )}
          <div className="flex-1" />
          {canStop ? (
            <Button size="icon" className="size-8 rounded-full" aria-label="Stop" onClick={() => void stop(session.id)}>
              <Square className="size-3.5 fill-current" />
            </Button>
          ) : (
            <Button
              size="icon"
              className="size-8 rounded-full disabled:bg-[#d9d9d9] disabled:opacity-100"
              disabled={!draft.trim() || running}
              onClick={submit}
            >
              <ArrowUp className="size-4" />
            </Button>
          )}
        </div>
      </div>
      <div className="mt-2.5 flex flex-wrap items-center gap-1.5 px-1">
        <RunnerPicker session={session} />
        <ModelChip session={session} />
        <FolderChip session={session} />
        <DataChip session={session} />
      </div>
    </div>
  );
}

/** Pending drops, shown above the textarea as removable chips until the next message ships them. */
export function AttachmentChips({ session }: { session: Session }) {
  const removeAttachment = useAppStore((s) => s.removeAttachment);
  if (session.runner === 'cloud' || session.attachments.length === 0) return null;
  return (
    <div className="mb-2 flex flex-wrap gap-1.5 px-1">
      {session.attachments.map((p) => (
        <span
          key={p}
          className="flex max-w-[220px] items-center gap-1.5 rounded-full bg-secondary py-1 pr-1.5 pl-2.5 text-[11.5px] text-secondary-foreground"
        >
          {p.startsWith('agentarea://files/') ? (
            <Cloud className="size-3 shrink-0 text-muted-foreground" aria-label="AgentArea workspace file" />
          ) : (
            <Paperclip className="size-3 shrink-0 text-muted-foreground" />
          )}
          <span className="truncate">{basename(p)}</span>
          <button
            onClick={() => removeAttachment(session.id, p)}
            aria-label={`Remove ${basename(p)}`}
            className="shrink-0 rounded-full p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
          >
            <X className="size-3" />
          </button>
        </span>
      ))}
    </div>
  );
}

export function RunnerPicker({ session }: { session: Session }) {
  const agents = useAppStore((s) => s.agents);
  const connected = useAppStore((s) => s.connection === 'connected');
  const setRunner = useAppStore((s) => s.setRunner);
  // A thread keeps its runner: the next turn resumes that runner's own session.
  const locked = session.resumeId !== null || session.messages.some((m) => m.role === 'user');
  const label =
    session.runner === 'cloud' ? `Cloud · ${session.agent?.name ?? 'agent'}` : `Local · ${RUNNER_LABEL[session.runner]}`;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={locked}>
        <button className="flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:hover:bg-transparent">
          <RunnerBadge runner={session.runner} className="size-3.5" />
          {label}
          {!locked && <ChevronDown className="size-3 opacity-60" />}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-64">
        <DropdownMenuLabel>Run locally</DropdownMenuLabel>
        <DropdownMenuItem onSelect={() => setRunner(session.id, 'claude')}>
          <ClaudeIcon className="size-3.5" /> Claude Code
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={() => setRunner(session.id, 'codex')}>
          <CodexIcon className="size-3.5" /> Codex
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuLabel>Delegate to AgentArea</DropdownMenuLabel>
        {!connected && <div className="px-2.5 py-1.5 text-xs text-muted-foreground">Connect to AgentArea first</div>}
        {connected && agents.length === 0 && (
          <div className="px-2.5 py-1.5 text-xs text-muted-foreground">No agents in the workspace</div>
        )}
        {connected &&
          agents.map((a) => (
            <DropdownMenuItem key={a.id} onSelect={() => setRunner(session.id, 'cloud', a)}>
              <Cloud /> <span className="truncate">{a.name}</span>
            </DropdownMenuItem>
          ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** Folder the local agent works in. Fixed once the thread has a user message; hidden for cloud. */
export function FolderChip({ session }: { session: Session }) {
  const pickFolder = useAppStore((s) => s.pickFolder);
  const setFolder = useAppStore((s) => s.setFolder);
  const recent = useAppStore((s) => s.recentFolders);
  if (session.runner === 'cloud') return null;
  const locked = session.messages.some((m) => m.role === 'user');
  const name = session.cwd ? basename(session.cwd) : 'Thread folder';

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={locked}>
        <button className="flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:hover:bg-transparent">
          <Folder className="size-3.5" />
          <span className="max-w-[160px] truncate">{name}</span>
          {!locked && <ChevronDown className="size-3 opacity-60" />}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-56">
        <DropdownMenuItem onSelect={() => void pickFolder(session.id)}>
          <FolderOpen /> Choose folder…
        </DropdownMenuItem>
        <DropdownMenuItem disabled={!session.cwd} onSelect={() => void pickFolder(session.id, true)}>
          <RotateCcw /> Use thread folder
        </DropdownMenuItem>
        {recent.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuLabel>Recent</DropdownMenuLabel>
            {recent.map((p) => (
              <DropdownMenuItem key={p} onSelect={() => setFolder(session.id, p)} title={p}>
                <Clock /> <span className="truncate">{basename(p)}</span>
              </DropdownMenuItem>
            ))}
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

interface DataSource {
  name: string;
  kind: 'local' | 'cloud';
  location: string;
  description: string;
}

let home: Promise<string> | null = null;
/** `/Users/me/x` → `~/x`, for display. */
function useTilde(path: string): string {
  const [dir, setDir] = useState<string | null>(null);
  useEffect(() => {
    home ??= import('@tauri-apps/api/path').then((m) => m.homeDir()).catch(() => '');
    void home.then(setDir);
  }, []);
  return dir && path.startsWith(dir) ? `~${path.slice(dir.replace(/\/+$/, '').length)}` : path;
}

function SourceRow({ icon: Icon, title, detail }: { icon: typeof Cloud; title: string; detail: string }) {
  return (
    <div className="flex items-start gap-2 px-2 py-1.5 text-[13px]" title={detail}>
      <Icon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
      <div className="min-w-0">
        <div className="truncate">{title}</div>
        <div className="truncate text-[11px] text-muted-foreground">{detail}</div>
      </div>
    </div>
  );
}

function LocalRow({ source }: { source: DataSource }) {
  return <SourceRow icon={Laptop} title={source.name} detail={useTilde(source.location)} />;
}

/**
 * What the local agent can read this turn: plugin folders and the thread's
 * folder (read directly), plus the AgentArea workspace's files (through the
 * agentarea_data tools) when signed in. Read-only; the list comes from the
 * same Rust code that builds the agent's sources.
 */
export function DataChip({ session }: { session: Session }) {
  const plugins = usePlugins((s) => s.plugins);
  const signedIn = useAppStore((s) => s.auth.status === 'authenticated');
  const workspace = useAppStore((s) => s.workspaces.find((w) => w.id === s.workspaceId)?.name ?? null);
  const [local, setLocal] = useState<DataSource[]>([]);
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (session.runner === 'cloud') return;
    let live = true;
    void import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke<DataSource[]>('data_sources', { sessionId: session.id, cwd: session.cwd }))
      .then((list) => live && setLocal(list))
      .catch(() => live && setLocal([]));
    return () => {
      live = false;
    };
  }, [session.id, session.cwd, session.runner, plugins, open]);
  if (session.runner === 'cloud') return null;
  const granted = local.filter((s) => s.name !== 'Thread folder');
  const hints = plugins.filter((p) => p.enabled && p.permissions.includes('cloud') && p.cloudRoot !== null);
  const label = `Data · ${granted.length} local${signedIn ? ' · 1 cloud' : ''}`;

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <button className="flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground">
          <Library className="size-3.5" />
          <span className="max-w-[180px] truncate">{label}</span>
          <ChevronDown className="size-3 opacity-60" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-80">
        <DropdownMenuLabel>On this device · read directly</DropdownMenuLabel>
        {local.map((s) => (
          <LocalRow key={s.location + s.name} source={s} />
        ))}
        <DropdownMenuSeparator />
        <DropdownMenuLabel>AgentArea · via agentarea_data</DropdownMenuLabel>
        {signedIn ? (
          <SourceRow
            icon={Cloud}
            title={`${workspace ?? 'Workspace'} files`}
            detail={
              hints.length
                ? hints.map((p) => `${p.title}: ${p.cloudRoot || '/'}`).join(' · ')
                : 'The workspace library'
            }
          />
        ) : (
          <div className="px-2 py-1.5 text-[12px] text-muted-foreground">Sign in to include cloud files</div>
        )}
        <DropdownMenuSeparator />
        <div className="px-2 py-1.5 text-[11px] text-muted-foreground">
          The agent sees these on every turn. Plugin folders are read-only.
        </div>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/**
 * Model + thinking level for a local thread, like Codex's model chip. Stays
 * changeable during the thread; "Default" leaves it to the CLI's own config.
 */
export function ModelChip({ session }: { session: Session }) {
  const codexModels = useAppStore((s) => s.codexModels);
  const setModel = useAppStore((s) => s.setModel);
  const setEffort = useAppStore((s) => s.setEffort);
  if (session.runner === 'cloud') return null;
  const models = modelsFor(session.runner, codexModels);
  const model = models.find((m) => m.id === session.model) ?? null;
  const efforts = model?.efforts ?? models[0]?.efforts ?? [];
  const label = `${model?.name ?? 'Default model'}${session.effort ? ` · ${session.effort}` : ''}`;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button className="flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground">
          <Brain className="size-3.5" />
          <span className="max-w-[180px] truncate">{label}</span>
          <ChevronDown className="size-3 opacity-60" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-96 w-60 overflow-y-auto">
        <DropdownMenuLabel>Model</DropdownMenuLabel>
        <DropdownMenuRadioGroup
          value={session.model ?? ''}
          onValueChange={(v) => setModel(session.id, v || null)}
        >
          <DropdownMenuRadioItem value="">Default</DropdownMenuRadioItem>
          {models.map((m) => (
            <DropdownMenuRadioItem key={m.id} value={m.id}>
              {m.name}
            </DropdownMenuRadioItem>
          ))}
        </DropdownMenuRadioGroup>
        {models.length === 0 && (
          <div className="px-2.5 py-1.5 text-xs text-muted-foreground">Run Codex once to load its models</div>
        )}
        {efforts.length > 0 && (
          <>
            <DropdownMenuSeparator />
            <DropdownMenuLabel>Thinking</DropdownMenuLabel>
            <DropdownMenuRadioGroup
              value={session.effort ?? ''}
              onValueChange={(v) => setEffort(session.id, v || null)}
            >
              <DropdownMenuRadioItem value="">
                Default{model?.defaultEffort ? ` (${model.defaultEffort})` : ''}
              </DropdownMenuRadioItem>
              {efforts.map((e) => (
                <DropdownMenuRadioItem key={e} value={e} className="capitalize">
                  {e}
                </DropdownMenuRadioItem>
              ))}
            </DropdownMenuRadioGroup>
          </>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function MemberPicker({
  session,
  kind,
  label,
  icon: Icon,
  items,
  selected,
}: {
  session: Session;
  kind: MemberKind;
  label: string;
  icon: typeof Plug;
  items: CatalogItem[];
  selected: string[];
}) {
  const toggleMember = useAppStore((s) => s.toggleMember);
  // Picks are stored on the control plane, so they need a live connection.
  const connected = useAppStore((s) => s.connection === 'connected');
  const workspace = useAppStore((s) => s.workspaces.find((w) => w.id === s.workspaceId)?.name);
  const picked = items.filter((it) => selected.includes(it.id));
  // Skills: the workspace's own first, then the platform built-ins every workspace shares.
  const groups = [
    { title: workspace ? `In ${workspace}` : `${label} in this workspace`, list: items.filter((it) => !it.builtin) },
    { title: 'Built-in', list: items.filter((it) => it.builtin) },
  ].filter((g) => g.list.length > 0);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild disabled={!connected}>
        <button className="flex items-center gap-1.5 rounded-full px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground disabled:opacity-50 disabled:hover:bg-transparent">
          {picked.length > 0 ? (
            <span className="flex -space-x-1">
              {picked.slice(0, 3).map((it) => (
                <EntityIcon key={it.id} name={it.name} icons={it.icons} seed={it.id} className="size-4 ring-2 ring-background" />
              ))}
            </span>
          ) : (
            <Icon className="size-3.5" />
          )}
          {label}
          {selected.length > 0 && (
            <span className="rounded-full bg-accent px-1.5 text-[11px] font-medium text-foreground">
              {selected.length}
            </span>
          )}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="max-h-96 w-72 overflow-y-auto">
        {items.length === 0 && <div className="px-2.5 py-1.5 text-xs text-muted-foreground">Nothing in the workspace yet</div>}
        {groups.map((g, gi) => (
          <div key={g.title}>
            {gi > 0 && <DropdownMenuSeparator />}
            <DropdownMenuLabel>{g.title}</DropdownMenuLabel>
            {g.list.map((it) => (
              <DropdownMenuCheckboxItem
                key={it.id}
                checked={selected.includes(it.id)}
                onSelect={(e) => e.preventDefault()}
                onCheckedChange={() => void toggleMember(session.id, kind, it.id)}
              >
                <EntityIcon name={it.name} icons={it.icons} seed={it.id} />
                <div className="min-w-0">
                  <div className="truncate">{it.name}</div>
                  {it.description && (
                    <div className="truncate text-[11px] font-normal text-muted-foreground">{it.description}</div>
                  )}
                </div>
              </DropdownMenuCheckboxItem>
            ))}
          </div>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
