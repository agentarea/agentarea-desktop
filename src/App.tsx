import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react';
import { Moon, Sun, LogOut, X, SquarePen, AppWindow, Puzzle, Inbox, KeyRound, ChevronDown, ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { LoginScreen } from '@/components/LoginScreen';
import { Composer } from '@/components/Composer';
import { AgentGrid } from '@/components/AgentGrid';
import { AppsPanel, PluginsPanel } from '@/components/AppsPanel';
import { InboxView } from '@/components/InboxView';
import { AppView } from '@/components/AppView';
import { EntityIcon } from '@/components/EntityIcon';
import { usePinnedApps } from '@/lib/pinnedApps';
import { SecretsView } from '@/components/SecretsView';
import { localInbox } from '@/lib/inbox';
import { PluginFrame } from '@/components/PluginFrame';
import { usePlugins } from '@/lib/plugins';
import { useDragDropTarget, DropOverlay } from '@/components/DropZone';
import { ThreadFolder } from '@/components/FileChip';
import { ApprovalCard, DraftRow, groupMessages, MessageRow, ToolGroup } from '@/components/MessageView';
import { RunnerBadge, RUNNER_LABEL } from '@/assets/icons';
import { useAppStore } from '@/store';
import type { Session } from './data';

export default function App() {
  const theme = useAppStore((s) => s.settings.theme);
  const authStatus = useAppStore((s) => s.auth.status);

  return (
    <div className={cn('h-full', theme === 'dark' && 'dark')}>
      {authStatus === 'authenticated' ? <Workspace /> : <LoginScreen />}
    </div>
  );
}

/* ── View: threads (in 1–4 panes), a panel, or a plugin's own page ───── */

/** 'plugin-page': a page a plugin opened (ui.open), full-width; not persisted. */
type ViewMode = 'threads' | 'inbox' | 'apps' | 'plugins' | 'secrets' | 'plugin-page' | 'app-page';
type Panes = 1 | 2 | 3 | 4;
const VIEW_KEY = 'aa.view';
const PANES_KEY = 'aa.panes';

function load<T>(key: string, valid: readonly T[], fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    const v = (raw !== null && /^\d+$/.test(raw) ? Number(raw) : raw) as T;
    return valid.includes(v) ? v : fallback;
  } catch {
    return fallback;
  }
}

function usePersisted<T>(key: string, valid: readonly T[], fallback: T): [T, (v: T) => void] {
  const [value, setValue] = useState<T>(() => load(key, valid, fallback));
  const set = (v: T) => {
    setValue(v);
    try {
      localStorage.setItem(key, String(v));
    } catch {
      /* best-effort */
    }
  };
  return [value, set];
}

function Workspace() {
  const sessions = useAppStore((s) => s.sessions);
  const selected = useAppStore((s) => s.sessions.find((x) => x.id === s.selectedId) ?? null);
  const connected = useAppStore((s) => s.connection === 'connected');
  const select = useAppStore((s) => s.select);
  const newSession = useAppStore((s) => s.newSession);
  const [view, setView] = usePersisted<ViewMode>(VIEW_KEY, ['threads', 'inbox', 'apps', 'plugins', 'secrets'], 'threads');
  const [panes, setPanes] = usePersisted<Panes>(PANES_KEY, [1, 2, 3, 4], 1);
  // Panes are numbered slots (like tmux). A slot keeps its thread until you
  // put another one there; empty slots fill with threads not on screen.
  const [slots, setSlots] = useState<string[]>([]);
  const [activeSlot, setActiveSlot] = useState(0);
  // Pane count to return to after enlarging one thread (⌘↵ / Esc).
  const [zoomedFrom, setZoomedFrom] = useState<Panes | null>(null);
  const grid = view === 'threads' && panes > 1;
  const overId = useDragDropTarget(selected?.id ?? null, grid);

  const visible: Session[] = [];
  {
    const pinned = new Set(slots.slice(0, panes));
    const spare = sessions.filter((s) => !pinned.has(s.id));
    for (let i = 0; i < panes; i++) {
      const s = sessions.find((x) => x.id === slots[i]) ?? spare.shift();
      if (s) visible.push(s);
    }
  }
  const slotOf = grid ? Object.fromEntries(visible.map((s, i) => [s.id, i + 1])) : {};
  const active = Math.min(activeSlot, Math.max(visible.length - 1, 0));

  // Whatever gets selected (sidebar click, new thread, plugin) and is not on
  // screen lands in the active slot; if it is on screen, its slot becomes active.
  useEffect(() => {
    if (!grid || !selected) return;
    const i = visible.findIndex((s) => s.id === selected.id);
    if (i >= 0) return setActiveSlot(i);
    setSlots(() => {
      const next = visible.map((s) => s.id);
      next[active] = selected.id;
      return next;
    });
  }, [selected?.id, grid]);

  // One pane opens straight into a thread; several panes are fine with none.
  useEffect(() => {
    if (view === 'threads' && panes === 1 && !selected) void newSession();
  }, [view, panes, selected, newSession]);

  const openThread = (id: string) => {
    select(id);
    setView('threads');
  };

  // Seeing a thread's latest messages takes it out of the inbox.
  const markRead = useAppStore((s) => s.markRead);
  useEffect(() => {
    if (view === 'threads' && selected) markRead(selected.id);
  }, [view, selected?.id, selected?.messages.length, markRead]);

  // Cloud tasks change without us: poll the AgentArea inbox while connected.
  const loadInbox = useAppStore((s) => s.loadInbox);
  useEffect(() => {
    if (!connected) return;
    void loadInbox();
    const t = setInterval(() => void loadInbox(), 30_000);
    return () => clearInterval(t);
  }, [connected, loadInbox]);
  const zoomIn = (id: string | null) => {
    if (id) select(id);
    if (panes > 1) setZoomedFrom(panes);
    setPanes(1);
    setView('threads');
  };
  const zoomOut = () => {
    if (zoomedFrom) setPanes(zoomedFrom);
    setZoomedFrom(null);
  };

  // A pinned app clicked in the sidebar: show it full-width.
  const openedApp = usePinnedApps((s) => s.opened);
  const pinnedApps = usePinnedApps((s) => s.pinned);
  const shownApp = openedApp ? pinnedApps.find((a) => a.key === openedApp.key) : undefined;
  useEffect(() => {
    if (openedApp) setView('app-page');
  }, [openedApp]);

  // A plugin's sidebar opened its page (ui.open): show just that page, not
  // the Plugins management panel.
  const pluginTarget = usePlugins((s) => s.target);
  const targetPlugin = usePlugins((s) => s.plugins.find((p) => p.id === s.target?.id) ?? null);
  useEffect(() => {
    if (pluginTarget) setView('plugin-page');
  }, [pluginTarget]);
  useEffect(() => {
    void usePlugins.getState().reload();
  }, []);

  useHotkeys({
    // ⌘T: a new thread in a new pane while there is room, otherwise in the
    // active one — like a new browser tab — with its input focused.
    newThread: () => {
      if (grid && panes < 4) {
        setActiveSlot(panes);
        setPanes((panes + 1) as Panes);
      }
      newThread();
      setTimeout(() => {
        const id = useAppStore.getState().selectedId;
        const el = document.querySelector<HTMLTextAreaElement>(
          id ? `[data-session-id="${id}"] textarea, main textarea` : 'main textarea',
        );
        el?.focus();
      }, 50);
    },
    panes: (n) => {
      setZoomedFrom(null);
      setPanes(n);
      setView('threads');
    },
    toggleZoom: () => (panes === 1 && zoomedFrom ? zoomOut() : zoomIn(selected?.id ?? null)),
    escape: () => panes === 1 && zoomedFrom && zoomOut(),
    step: (dir) => {
      // In panes, move between slots; in one pane, go through all threads.
      const list = grid ? visible : sessions;
      if (!list.length) return;
      const i = list.findIndex((s) => s.id === selected?.id);
      const next = list[(i + dir + list.length) % list.length];
      if (grid) select(next.id);
      else openThread(next.id);
    },
  });
  const newThread = () => {
    void newSession();
    setView('threads');
  };

  return (
    <div className="flex h-full bg-background text-foreground">
      <WorkspaceRail />
      <Sidebar view={view} slotOf={slotOf} onChangeView={setView} onOpenThread={openThread} onNewThread={newThread} />
      <main className="flex min-w-0 flex-1 flex-col bg-background">
        <MainHeader
          title={
            view === 'app-page'
              ? shownApp?.title
              : view === 'inbox'
              ? 'Inbox'
              : view === 'apps'
              ? 'Apps'
              : view === 'plugins'
                ? 'Plugins'
                : view === 'secrets'
                  ? 'Secrets'
                : view === 'plugin-page'
                  ? targetPlugin?.title
                  : !grid && view === 'threads' && selected && selected.messages.length > 0
                  ? selected.title
                  : undefined
          }
          panes={view === 'threads' ? panes : null}
          zoomed={zoomedFrom !== null}
          onChangePanes={(n) => {
            setZoomedFrom(null);
            setPanes(n);
            setView('threads');
          }}
        />
        {grid && (
          <AgentGrid
            sessions={visible}
            panes={panes as 2 | 3 | 4}
            activeId={selected?.id ?? null}
            overId={overId}
            onActivate={select}
            onOpenThread={zoomIn}
            onNewThread={newThread}
          />
        )}
        {view === 'threads' && panes === 1 && selected && <SessionBody session={selected} overId={overId} />}
        {view === 'inbox' && <InboxView onOpenThread={openThread} />}
        {view === 'app-page' &&
          (shownApp ? (
            <AppView key={shownApp.key} app={shownApp} />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
              This app was unpinned. Open it from Apps.
            </div>
          ))}
        {view === 'apps' && <AppsPanel connected={connected} />}
        {view === 'plugins' && <PluginsPanel />}
        {view === 'secrets' && <SecretsView />}
        {view === 'plugin-page' &&
          (targetPlugin?.enabled && pluginTarget ? (
            <PluginFrame key={targetPlugin.id} plugin={targetPlugin} hash={pluginTarget.hash} />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">This plugin is off or gone.</div>
          ))}
      </main>
    </div>
  );
}

function MainHeader({
  title,
  panes,
  zoomed,
  onChangePanes,
}: {
  title?: string;
  /** one thread enlarged from panes: Esc / ⌘↵ goes back */
  zoomed: boolean;
  /** current pane count, or null when a panel (Apps/Plugins) is open */
  panes: Panes | null;
  onChangePanes: (n: Panes) => void;
}) {
  return (
    <div data-tauri-drag-region className="relative flex h-[52px] shrink-0 items-center justify-center px-20">
      {title && <span className="truncate text-[13px] font-medium text-foreground/80">{title}</span>}
      {zoomed && <span className="ml-2 shrink-0 text-[11px] text-muted-foreground-subtle">Esc to go back</span>}
      <div className="absolute top-1/2 right-4 -translate-y-1/2" style={{ WebkitAppRegion: 'no-drag' } as CSSProperties}>
        <TooltipProvider>
          <div className="flex items-center gap-0.5 rounded-full border border-border bg-secondary/70 p-0.5">
            {([1, 2, 3, 4] as const).map((n) => (
              <Tooltip key={n}>
                <TooltipTrigger asChild>
                  <button
                    onClick={() => onChangePanes(n)}
                    aria-label={`${n} ${n === 1 ? 'pane' : 'panes'}`}
                    aria-pressed={panes === n}
                    className={cn(
                      'flex size-6 items-center justify-center rounded-full transition-colors',
                      panes === n
                        ? 'bg-background text-foreground shadow-[0_1px_4px_rgba(0,0,0,0.08)]'
                        : 'text-muted-foreground hover:text-foreground',
                    )}
                  >
                    <PanesIcon n={n} />
                  </button>
                </TooltipTrigger>
                <TooltipContent side="bottom">
                  {n === 1 ? 'One thread' : `${n} threads side by side`} · ⌘{n}
                </TooltipContent>
              </Tooltip>
            ))}
          </div>
        </TooltipProvider>
      </div>
    </div>
  );
}

/**
 * Window-level shortcuts: ⌘T new thread, ⌘1–⌘4 pane count, ⌘↵ enlarge/restore the active
 * thread, Esc restore, ⌘[ / ⌘] previous/next thread. Handlers are read from a
 * ref so the listener is registered once.
 */
function useHotkeys(h: {
  newThread: () => void;
  panes: (n: Panes) => void;
  toggleZoom: () => void;
  escape: () => void;
  step: (dir: 1 | -1) => void;
}) {
  const ref = useRef(h);
  ref.current = h;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (e.ctrlKey && !e.metaKey && /^[1-9]$/.test(e.key)) {
        // ⌃1–⌃9: workspaces, like Slack's ⌘1–⌘9 (⌘ digits are panes here).
        const ws = useAppStore.getState().workspaces[Number(e.key) - 1];
        if (ws) {
          e.preventDefault();
          void useAppStore.getState().setWorkspace(ws.id);
        }
      } else if (mod && !e.shiftKey && e.key.toLowerCase() === 't') {
        e.preventDefault();
        ref.current.newThread();
      } else if (e.metaKey && ['1', '2', '3', '4'].includes(e.key)) {
        e.preventDefault();
        ref.current.panes(Number(e.key) as Panes);
      } else if (mod && e.key === 'Enter') {
        e.preventDefault();
        ref.current.toggleZoom();
      } else if (mod && (e.key === ']' || e.key === '[')) {
        e.preventDefault();
        ref.current.step(e.key === ']' ? 1 : -1);
      } else if (e.key === 'Escape' && !e.defaultPrevented) {
        // Menus and dialogs handle Escape first and mark it handled.
        ref.current.escape();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
}

/** A tiny picture of the layout: 1 box, 2 or 3 columns, or a 2×2 grid. */
function PanesIcon({ n }: { n: Panes }) {
  const rects =
    n === 1
      ? [[1, 1, 12, 12]]
      : n === 2
        ? [[1, 1, 5.5, 12], [7.5, 1, 5.5, 12]]
        : n === 3
          ? [[1, 1, 3.3, 12], [5.35, 1, 3.3, 12], [9.7, 1, 3.3, 12]]
          : [[1, 1, 5.5, 5.5], [7.5, 1, 5.5, 5.5], [1, 7.5, 5.5, 5.5], [7.5, 7.5, 5.5, 5.5]];
  return (
    <svg viewBox="0 0 14 14" className="size-3.5" fill="none" stroke="currentColor" strokeWidth="1.2">
      {rects.map(([x, y, w, h], i) => (
        <rect key={i} x={x} y={y} width={w} height={h} rx="1.2" />
      ))}
    </svg>
  );
}

/* ── Workspace rail: switch orgs like Slack ─────────── */

function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  return ((words[0]?.[0] ?? '?') + (words[1]?.[0] ?? '')).toUpperCase();
}

/** Stable, muted colour per workspace so the squares are told apart at a glance. */
function hue(id: string): number {
  let h = 0;
  for (const c of id) h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}

function WorkspaceRail() {
  const workspaces = useAppStore((s) => s.workspaces);
  const workspaceId = useAppStore((s) => s.workspaceId);
  const setWorkspace = useAppStore((s) => s.setWorkspace);
  // One workspace needs no switcher.
  if (workspaces.length < 2) return null;

  return (
    <nav className="flex w-[60px] shrink-0 flex-col items-center gap-2 border-r border-border bg-sidebar pt-[52px] pb-3">
      <TooltipProvider>
        {workspaces.map((w, i) => {
          const active = w.id === workspaceId;
          return (
            <Tooltip key={w.id}>
              <TooltipTrigger asChild>
                <button
                  onClick={() => void setWorkspace(w.id)}
                  aria-label={w.name}
                  aria-current={active}
                  className="group relative flex items-center"
                >
                  <span
                    className={cn(
                      'absolute -left-[14px] w-1 rounded-r-full bg-foreground transition-all',
                      active ? 'h-6' : 'h-0 group-hover:h-2.5',
                    )}
                  />
                  <span
                    style={{ backgroundColor: `hsl(${hue(w.id)} 30% ${active ? 38 : 55}%)` }}
                    className={cn(
                      'flex size-9 items-center justify-center rounded-xl text-[12px] font-semibold text-white transition-all',
                      active ? 'ring-2 ring-foreground/15 ring-offset-2 ring-offset-sidebar' : 'opacity-80 group-hover:opacity-100',
                    )}
                  >
                    {initials(w.name)}
                  </span>
                </button>
              </TooltipTrigger>
              <TooltipContent side="right">
                {w.name}
                {i < 9 && <span className="ml-1.5 opacity-60">⌃{i + 1}</span>}
              </TooltipContent>
            </Tooltip>
          );
        })}
      </TooltipProvider>
    </nav>
  );
}

/* ── Sidebar: threads ────────────────────────────────── */

function Sidebar({
  view,
  slotOf,
  onChangeView,
  onOpenThread,
  onNewThread,
}: {
  view: ViewMode;
  /** thread id → pane number, for threads on screen in a multi-pane layout */
  slotOf: Record<string, number>;
  onChangeView: (v: ViewMode) => void;
  onOpenThread: (id: string) => void;
  onNewThread: () => void;
}) {
  const sessions = useAppStore((s) => s.sessions);
  const selectedId = useAppStore((s) => s.selectedId);
  const running = useAppStore((s) => s.running);
  const deleteSession = useAppStore((s) => s.deleteSession);
  const inThreads = view === 'threads';
  const cloudInbox = useAppStore((s) => s.cloudInbox);
  const approvals = useAppStore((s) => s.approvals);
  const inboxCount = useMemo(
    () => localInbox(sessions, running, approvals).length + cloudInbox.filter((i) => i.status !== 'completed').length,
    [sessions, running, approvals, cloudInbox],
  );

  return (
    <aside className="flex w-[260px] shrink-0 flex-col bg-sidebar text-sidebar-foreground">
      {/* Drag handle — macOS traffic lights float over this strip (see hiddenTitle/titleBarStyle in tauri.conf.json). */}
      <div data-tauri-drag-region className="h-[52px] shrink-0 pl-20" />

      <div className="px-2.5">
        <button
          onClick={onNewThread}
          className="flex w-full items-center gap-2 rounded-lg px-2.5 py-[7px] text-[13px] font-medium text-foreground/85 transition-colors hover:bg-sidebar-hover"
        >
          <SquarePen className="size-[15px] text-muted-foreground" />
          New thread
          <kbd className="ml-auto text-[11px] font-normal text-muted-foreground-subtle">⌘T</kbd>
        </button>
        <SidebarNavItem
          icon={Inbox}
          label="Inbox"
          count={inboxCount}
          active={view === 'inbox'}
          onClick={() => onChangeView('inbox')}
        />
        <SidebarNavItem icon={AppWindow} label="Apps" active={view === 'apps'} onClick={() => onChangeView('apps')} />
        <SidebarNavItem icon={Puzzle} label="Plugins" active={view === 'plugins'} onClick={() => onChangeView('plugins')} />
        <SidebarNavItem icon={KeyRound} label="Secrets" active={view === 'secrets'} onClick={() => onChangeView('secrets')} />
        <PinnedAppsSection active={view === 'app-page'} />
      </div>

      <div className="mt-4 px-4 pb-1.5 text-[11px] font-medium text-muted-foreground-subtle">Threads</div>

      <ScrollArea className="min-h-0 flex-1 px-2.5">
        <div className="flex flex-col gap-px pb-2">
          {sessions.map((s) => (
            <div
              key={s.id}
              onClick={() => onOpenThread(s.id)}
              className={cn(
                'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[7px] transition-colors',
                inThreads && selectedId === s.id ? 'bg-sidebar-accent' : 'hover:bg-sidebar-hover',
              )}
            >
              <RunnerBadge runner={s.runner} className="size-3.5 shrink-0 text-muted-foreground" />
              {slotOf[s.id] && (
                <span
                  title={`In pane ${slotOf[s.id]}`}
                  className="flex size-4 shrink-0 items-center justify-center rounded bg-foreground/[0.07] text-[10px] font-medium text-foreground/70"
                >
                  {slotOf[s.id]}
                </span>
              )}
              <span className="min-w-0 flex-1 truncate text-[13px] leading-tight">{s.title}</span>
              {approvals[s.id]?.length ? (
                <span
                  title="Waiting for your approval"
                  className="shrink-0 rounded-full bg-amber-500/15 px-1.5 text-[10.5px] font-medium text-amber-700"
                >
                  {approvals[s.id].length}
                </span>
              ) : running[s.id] ? (
                <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-chart-2" />
              ) : (
                <span className="shrink-0 text-[11px] text-muted-foreground-subtle group-hover:hidden">
                  {s.runner === 'cloud' ? (s.agent?.name ?? 'Cloud') : RUNNER_LABEL[s.runner]}
                </span>
              )}
              <button
                onClick={(e) => {
                  e.stopPropagation();
                  void deleteSession(s.id);
                }}
                className="hidden shrink-0 text-muted-foreground hover:text-foreground group-hover:block"
                aria-label="Delete thread"
              >
                <X className="size-3.5" />
              </button>
            </div>
          ))}
        </div>
      </ScrollArea>

      <PluginSidebarSections />

      <SidebarFooter />
    </aside>
  );
}

/** Quiet status row: connection + environment, theme toggle, sign out. No heavy top bar. */
/** Enabled plugins that ship a sidebar page get a collapsible section here. */
function PluginSidebarSections() {
  const all = usePlugins((s) => s.plugins);
  const plugins = useMemo(() => all.filter((p) => p.enabled && p.sidebar), [all]);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() => {
    try {
      return JSON.parse(localStorage.getItem('aa.pluginSections') ?? '{}');
    } catch {
      return {};
    }
  });
  const toggle = (id: string) => {
    const next = { ...collapsed, [id]: !collapsed[id] };
    setCollapsed(next);
    try {
      localStorage.setItem('aa.pluginSections', JSON.stringify(next));
    } catch {
      /* best-effort */
    }
  };
  if (!plugins.length) return null;

  return (
    <div className="flex max-h-[45%] shrink-0 flex-col border-t border-border">
      {plugins.map((p) => (
        <div key={p.id} className={cn('flex min-h-0 flex-col', !collapsed[p.id] && 'flex-1')}>
          <button
            onClick={() => toggle(p.id)}
            className="flex shrink-0 items-center gap-1 px-4 pt-2.5 pb-1.5 text-[11px] font-medium text-muted-foreground-subtle hover:text-foreground"
          >
            {collapsed[p.id] ? <ChevronRight className="size-3" /> : <ChevronDown className="size-3" />}
            {p.title}
          </button>
          {!collapsed[p.id] && (
            <PluginFrame plugin={p} page={p.sidebar!} className="min-h-[180px] w-full flex-1 border-0 bg-sidebar" />
          )}
        </div>
      ))}
    </div>
  );
}

/** Apps pinned from the Apps panel, one click away. */
function PinnedAppsSection({ active }: { active: boolean }) {
  const pinned = usePinnedApps((s) => s.pinned);
  const opened = usePinnedApps((s) => s.opened);
  const open = usePinnedApps((s) => s.open);
  const unpin = usePinnedApps((s) => s.unpin);
  if (!pinned.length) return null;
  return (
    <div className="mt-3">
      <div className="px-2.5 pb-1 text-[11px] font-medium text-muted-foreground-subtle">Pinned</div>
      {pinned.map((a) => (
        <div
          key={a.key}
          onClick={() => open(a.key)}
          title={`${a.title} — ${a.kind === 'local' ? 'on this device' : 'AgentArea'} · ${a.subtitle}`}
          className={cn(
            'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[6px] text-[13px] transition-colors',
            active && opened?.key === a.key ? 'bg-sidebar-accent' : 'hover:bg-sidebar-hover',
          )}
        >
          <EntityIcon name={a.title} icons={a.icons} seed={a.seed} className="size-4 rounded-[5px] text-[7px]" />
          <span className="min-w-0 flex-1 truncate">{a.title}</span>
          <span className="shrink-0 text-[10.5px] text-muted-foreground-subtle group-hover:hidden">
            {a.kind === 'local' ? 'Local' : 'Cloud'}
          </span>
          <button
            onClick={(e) => {
              e.stopPropagation();
              unpin(a.key);
            }}
            className="hidden shrink-0 text-muted-foreground hover:text-foreground group-hover:block"
            aria-label={`Unpin ${a.title}`}
          >
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}

function SidebarNavItem({
  icon: Icon,
  label,
  count,
  active,
  onClick,
}: {
  icon: typeof AppWindow;
  label: string;
  /** badge, e.g. items waiting in the inbox */
  count?: number;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        'flex w-full items-center gap-2 rounded-lg px-2.5 py-[7px] text-[13px] font-medium text-foreground/85 transition-colors',
        active ? 'bg-sidebar-accent' : 'hover:bg-sidebar-hover',
      )}
    >
      <Icon className="size-[15px] text-muted-foreground" />
      {label}
      {!!count && (
        <span className="ml-auto rounded-full bg-foreground px-1.5 text-[10.5px] font-semibold text-background">{count}</span>
      )}
    </button>
  );
}

function SidebarFooter() {
  const theme = useAppStore((s) => s.settings.theme);
  const toggleTheme = useAppStore((s) => s.toggleTheme);
  const logout = useAppStore((s) => s.logout);
  const email = useAppStore((s) => s.auth.user?.email);

  return (
    <div className="flex shrink-0 items-center justify-between gap-1 border-t border-sidebar-border px-2 py-1.5">
      <ConnectionPill />
      <div className="flex shrink-0 items-center gap-0.5">
        <Button
          variant="ghost"
          size="icon"
          className="size-6 text-muted-foreground hover:text-foreground"
          onClick={toggleTheme}
        >
          {theme === 'dark' ? <Moon className="size-3.5" /> : <Sun className="size-3.5" />}
        </Button>
        <TooltipProvider>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="size-6 text-muted-foreground hover:text-foreground"
                onClick={logout}
              >
                <LogOut className="size-3.5" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="top">{email ? `Sign out ${email}` : 'Sign out'}</TooltipContent>
          </Tooltip>
        </TooltipProvider>
      </div>
    </div>
  );
}

function ConnectionPill() {
  const status = useAppStore((s) => s.connection);
  const envLabel = useAppStore((s) => s.env.label);
  const meta = {
    connected: { dot: 'bg-status-success', label: 'Connected', ping: false },
    connecting: { dot: 'bg-chart-2', label: 'Connecting…', ping: true },
    offline: { dot: 'bg-destructive', label: 'Offline', ping: false },
  }[status];
  return (
    <div className="flex min-w-0 items-center gap-1.5 px-1 text-[11px] text-muted-foreground">
      <span className="relative flex size-1.5 shrink-0">
        {meta.ping && (
          <span className={cn('absolute inline-flex size-full animate-ping rounded-full opacity-60', meta.dot)} />
        )}
        <span className={cn('relative inline-flex size-1.5 rounded-full', meta.dot)} />
      </span>
      <span className="truncate">
        {envLabel} · {meta.label}
      </span>
    </div>
  );
}

/* ── Thread (Focus) ─────────────────────────────────────── */

const NO_APPROVALS: never[] = [];

function SessionBody({ session, overId }: { session: Session; overId: string | null }) {
  const running = useAppStore((s) => !!s.running[session.id]);
  const draft = useAppStore((s) => s.drafts[session.id] ?? '');
  const approvals = useAppStore((s) => s.approvals[session.id] ?? NO_APPROVALS);
  const bottom = useRef<HTMLDivElement>(null);
  const fresh = session.messages.length === 0;

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: 'end' });
  }, [session.messages.length, running, draft.length, approvals.length]);

  if (fresh)
    return (
      <div className="relative flex flex-1 flex-col items-center justify-center px-6 pb-24">
        <h1 className="mb-7 text-[26px] font-semibold tracking-tight text-foreground">What should we work on?</h1>
        <div className="w-full max-w-[600px]">
          <Composer session={session} autoFocus />
        </div>
        {overId === session.id && <DropOverlay />}
      </div>
    );

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <ScrollArea className="min-h-0 flex-1">
        <div className="mx-auto flex max-w-[720px] flex-col gap-5 px-6 py-6">
          <ThreadFolder session={session}>
            {groupMessages(session.messages).map((g) =>
              Array.isArray(g) ? <ToolGroup key={g[0].id} tools={g} /> : <MessageRow key={g.id} message={g} />,
            )}
            {draft && <DraftRow text={draft} />}
          </ThreadFolder>
          {approvals.map((a) => (
            <ApprovalCard key={a.id} approval={a} />
          ))}
          {running && !draft && approvals.length === 0 && (
            <div className="flex items-center gap-2 text-[13px] text-muted-foreground">
              <span className="size-1.5 animate-pulse rounded-full bg-chart-2" />
              Working…
            </div>
          )}
          <div ref={bottom} />
        </div>
      </ScrollArea>
      <div className="mx-auto w-full max-w-[720px] px-6 pb-5">
        <Composer session={session} />
      </div>
      {overId === session.id && <DropOverlay />}
    </div>
  );
}
