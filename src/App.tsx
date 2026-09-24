import { useEffect, useMemo, useRef, useState } from 'react';
import { Moon, Sun, LogOut, X, SquarePen, AppWindow, Puzzle, Inbox, KeyRound, ChevronDown, ChevronRight } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { LoginScreen } from '@/components/LoginScreen';
import { DragLayer, SplitView, startThreadDrag } from '@/components/SplitView';
import { AppsPanel, PluginsPanel } from '@/components/AppsPanel';
import { InboxView } from '@/components/InboxView';
import { AppView } from '@/components/AppView';
import { EntityIcon } from '@/components/EntityIcon';
import { usePinnedApps } from '@/lib/pinnedApps';
import { SecretsView } from '@/components/SecretsView';
import { localInbox } from '@/lib/inbox';
import { PluginFrame } from '@/components/PluginFrame';
import { usePlugins } from '@/lib/plugins';
import { useDragDropTarget } from '@/components/DropZone';
import { RunnerBadge, RUNNER_LABEL } from '@/assets/icons';
import { leaves, usePanes } from '@/lib/panes';
import { useAppStore } from '@/store';

export default function App() {
  const theme = useAppStore((s) => s.settings.theme);
  const authStatus = useAppStore((s) => s.auth.status);

  return (
    <div className={cn('h-full', theme === 'dark' && 'dark')}>
      {authStatus === 'authenticated' ? <Workspace /> : <LoginScreen />}
    </div>
  );
}

/* ── View: threads (one, or up to four split), a panel, or a plugin's own page ───── */

/** 'plugin-page': a page a plugin opened (ui.open), full-width; not persisted. */
type ViewMode = 'threads' | 'inbox' | 'apps' | 'plugins' | 'secrets' | 'plugin-page' | 'app-page';
const VIEW_KEY = 'aa.view';

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
  const tree = usePanes((s) => s.tree);
  const maximized = usePanes((s) => s.maximized);
  const paneList = leaves(tree);
  const split = view === 'threads' && paneList.length > 1 && !maximized;
  const openIds = useMemo(
    () => new Set(leaves(tree).flatMap((l) => (l.sessionId ? [l.sessionId] : []))),
    [tree],
  );
  const overId = useDragDropTarget(selected?.id ?? null);

  // Whatever gets selected (sidebar click, new thread, inbox, plugin) lands in
  // the focused pane, or focuses the pane already showing it.
  useEffect(() => {
    if (selected) usePanes.getState().show(selected.id);
  }, [selected?.id]);

  // Deleted threads (and another workspace's) leave their panes.
  useEffect(() => {
    usePanes.getState().prune(new Set(sessions.map((s) => s.id)));
  }, [sessions]);

  /** threads open in panes other than the focused one */
  const otherPaneIds = () => {
    const { tree, focusedKey } = usePanes.getState();
    return leaves(tree).flatMap((l) => (l.key !== focusedKey && l.sessionId ? [l.sessionId] : []));
  };

  // The focused pane always shows a thread. Read live: closing a deleted
  // thread's pane (above) may already have selected its neighbour.
  useEffect(() => {
    if (view === 'threads' && !useAppStore.getState().selectedId) void newSession(otherPaneIds());
  }, [view, selected, newSession]);

  const openThread = (id: string) => {
    select(id);
    usePanes.getState().show(id);
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

  // Focus the composer of the thread just opened.
  const focusComposer = () =>
    setTimeout(() => {
      const id = useAppStore.getState().selectedId;
      // One selector list would match whichever textarea comes first on the page.
      const el =
        (id && document.querySelector<HTMLTextAreaElement>(`[data-session-id="${id}"] textarea`)) ||
        document.querySelector<HTMLTextAreaElement>('main textarea');
      el?.focus();
    }, 50);
  const newThread = () => {
    void newSession(otherPaneIds());
    setView('threads');
  };

  useHotkeys({
    // ⌘T: a new thread in the focused pane.
    newThread: () => {
      newThread();
      focusComposer();
    },
    // ⌘\: a new thread beside the focused one, while there is room.
    splitRight: () => {
      const p = usePanes.getState();
      if (view !== 'threads' || !p.split(p.focusedKey, 'right')) return;
      void newSession(leaves(p.tree).flatMap((l) => (l.sessionId ? [l.sessionId] : [])));
      focusComposer();
    },
    closePane: () => view === 'threads' && usePanes.getState().close(usePanes.getState().focusedKey),
    focusPane: (n) => {
      const l = leaves(usePanes.getState().tree)[n - 1];
      if (!l) return;
      usePanes.getState().focus(l.key);
      setView('threads');
    },
    toggleMaximize: () => {
      if (view === 'threads') usePanes.getState().toggleMaximize();
    },
    escape: () => maximized && usePanes.getState().toggleMaximize(),
    step: (dir) => {
      // Split: move focus between panes. One pane: go through all threads.
      if (split) {
        const { focusedKey, focus } = usePanes.getState();
        const i = paneList.findIndex((l) => l.key === focusedKey);
        return focus(paneList[(i + dir + paneList.length) % paneList.length].key);
      }
      if (!sessions.length) return;
      const i = sessions.findIndex((s) => s.id === selected?.id);
      openThread(sessions[(i + dir + sessions.length) % sessions.length].id);
    },
  });

  return (
    <div className="flex h-full bg-background text-foreground">
      <WorkspaceRail />
      <Sidebar view={view} openIds={openIds} onChangeView={setView} onOpenThread={openThread} onNewThread={newThread} />
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
                  : !split && view === 'threads' && selected && selected.messages.length > 0
                  ? selected.title
                  : undefined
          }
          maximized={view === 'threads' && maximized !== null}
        />
        {view === 'threads' && <SplitView overId={overId} />}
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
      <DragLayer />
    </div>
  );
}

/** a pane enlarged from a split (`maximized`): Esc / ⌘↵ goes back */
function MainHeader({ title, maximized }: { title?: string; maximized: boolean }) {
  return (
    <div data-tauri-drag-region className="relative flex h-[52px] shrink-0 items-center justify-center px-20">
      {title && <span className="truncate text-[13px] font-medium text-foreground/80">{title}</span>}
      {maximized && <span className="ml-2 shrink-0 text-[11px] text-muted-foreground-subtle">Esc to go back</span>}
    </div>
  );
}

/**
 * Window-level shortcuts: ⌘T new thread in the focused pane, ⌘\ split it
 * with a new thread, ⌘W close the focused pane, ⌘1–⌘4 focus pane N, ⌘[ / ⌘]
 * previous/next pane, ⌘↵ maximize/restore the focused pane, Esc restore,
 * ⌃1–⌃9 workspaces. Handlers are read from a ref so the listener is
 * registered once.
 */
function useHotkeys(h: {
  newThread: () => void;
  splitRight: () => void;
  closePane: () => void;
  focusPane: (n: number) => void;
  toggleMaximize: () => void;
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
      } else if (mod && !e.shiftKey && (e.key === '\\' || e.code === 'Backslash')) {
        e.preventDefault();
        ref.current.splitRight();
      } else if (e.metaKey && !e.shiftKey && e.key.toLowerCase() === 'w') {
        // Always handled, so ⌘W never closes the window from a single pane.
        e.preventDefault();
        ref.current.closePane();
      } else if (e.metaKey && ['1', '2', '3', '4'].includes(e.key)) {
        e.preventDefault();
        ref.current.focusPane(Number(e.key));
      } else if (mod && e.key === 'Enter') {
        e.preventDefault();
        ref.current.toggleMaximize();
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
  openIds,
  onChangeView,
  onOpenThread,
  onNewThread,
}: {
  view: ViewMode;
  /** threads open in a pane */
  openIds: Set<string>;
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

      {/* Radix wraps content in a display:table div that grows with the longest
          title; make it a block so titles truncate and row badges stay visible. */}
      <ScrollArea className="min-h-0 flex-1 px-2.5" viewportClassName="[&>div]:!block">
        <div className="flex flex-col gap-px pb-2">
          {sessions.map((s) => (
            <div
              key={s.id}
              onClick={() => onOpenThread(s.id)}
              // Drag onto the main area to open it in a split.
              onPointerDown={(e) => startThreadDrag(e, s.id)}
              className={cn(
                'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[7px] transition-colors select-none',
                inThreads && selectedId === s.id
                  ? 'bg-sidebar-accent'
                  : inThreads && openIds.has(s.id)
                    ? 'bg-sidebar-accent/50 hover:bg-sidebar-hover'
                    : 'hover:bg-sidebar-hover',
              )}
            >
              <RunnerBadge runner={s.runner} className="size-3.5 shrink-0 text-muted-foreground" />
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
