import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  Moon,
  Sun,
  LogIn,
  LogOut,
  X,
  SquarePen,
  AppWindow,
  Inbox,
  KeyRound,
  LayoutGrid,
  Pencil,
  PanelLeft,
} from 'lucide-react';
import { cn } from '@/lib/utils';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { LoginScreen } from '@/components/LoginScreen';
import { UpdateBanner } from '@/components/UpdateBanner';
import { DragLayer, SplitView, startPaneDrag, startThreadDrag, useContentTitle } from '@/components/SplitView';
import { AppsPanel, useAppsAttention, useAppsScreen } from '@/components/AppsPanel';
import { InboxView } from '@/components/InboxView';
import { AppView } from '@/components/AppView';
import { EntityIcon } from '@/components/EntityIcon';
import { usePinnedApps } from '@/lib/pinnedApps';
import { SecretsView } from '@/components/SecretsView';
import { localInbox } from '@/lib/inbox';
import { PluginFrame } from '@/components/PluginFrame';
import { usePlugins } from '@/lib/plugins';
import { useLocalMcp } from '@/lib/localMcp';
import { useDragDropTarget } from '@/components/DropZone';
import { PluginEntry, SectionHeader, SectionOrder, SidebarDragLayer, SidebarSection } from '@/components/SidebarSections';
import { fillOrder, isCollapsed, isPluginSection, pluginSection, useSidebarLayout } from '@/lib/sidebarLayout';
import { RunnerBadge, RUNNER_LABEL } from '@/assets/icons';
import { leaves, threadIds, usePanes, type Node } from '@/lib/panes';
import { defaultGroupName, useGroups } from '@/lib/groups';
import { useAppStore } from '@/store';
import type { Workspace as WorkspaceInfo } from '@/lib/api';

export default function App() {
  const theme = useAppStore((s) => s.settings.theme);
  const authStatus = useAppStore((s) => s.auth.status);
  // On the root too: menus and tooltips are portalled outside the app's div.
  useEffect(() => {
    document.documentElement.classList.toggle('dark', theme === 'dark');
  }, [theme]);

  return (
    <div className={cn('h-full', theme === 'dark' && 'dark')}>
      {authStatus === 'authenticated' ? <Workspace /> : <LoginScreen />}
      <UpdateBanner />
    </div>
  );
}

/* ── View: threads (one, or up to four split), a panel, or a plugin's own page ───── */

/** 'plugin-page' / 'app-page': one app full-width (a plugin's page, an MCP app); not persisted. */
type ViewMode = 'threads' | 'inbox' | 'apps' | 'secrets' | 'plugin-page' | 'app-page';
const VIEW_KEY = 'aa.view';
// Plugins were their own view before they became apps.
try {
  if (localStorage.getItem(VIEW_KEY) === 'plugins') localStorage.setItem(VIEW_KEY, 'apps');
} catch {
  /* best-effort */
}
/** the left column (workspace rail + sidebar): 'open' or 'closed' (⌘B) */
const NAV_KEY = 'aa.sidebar';
const SIDEBAR_WIDTH = 260;
// Wide enough to hold the macOS traffic lights, which sit over its top.
const RAIL_WIDTH = 72;

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
  const [view, setView] = usePersisted<ViewMode>(VIEW_KEY, ['threads', 'inbox', 'apps', 'secrets'], 'threads');
  const tree = usePanes((s) => s.tree);
  const maximized = usePanes((s) => s.maximized);
  const paneList = leaves(tree);
  const split = view === 'threads' && paneList.length > 1 && !maximized;
  const openIds = useMemo(() => new Set(threadIds(tree)), [tree]);
  const sole = tree.kind === 'leaf' ? tree : maximized ? paneList.find((l) => l.key === maximized) : undefined;
  const soleTitle = useContentTitle(sole?.content ?? null);
  const groupOpen = useGroups((s) => !!s.activeId);
  const overId = useDragDropTarget(selected?.id ?? null);
  const [nav, setNav] = usePersisted<'open' | 'closed'>(NAV_KEY, ['open', 'closed'], 'open');
  const navOpen = nav === 'open';
  const toggleNav = () => setNav(navOpen ? 'closed' : 'open');
  const railShown = useAppStore((s) => s.workspaces.length > 1);

  // Whatever gets selected (sidebar click, new thread, inbox, plugin) lands in
  // the focused pane, or focuses the pane already showing it.
  useEffect(() => {
    if (selected) usePanes.getState().show(selected.id);
  }, [selected?.id]);

  // Threads just deleted (and another workspace's) leave their panes. Only
  // those: a restored group's long-gone thread keeps its placeholder pane.
  const knownIds = useRef<Set<string>>(new Set());
  useEffect(() => {
    const ids = new Set(sessions.map((s) => s.id));
    const gone = new Set([...knownIds.current].filter((id) => !ids.has(id)));
    knownIds.current = ids;
    if (gone.size) usePanes.getState().prune(gone);
  }, [sessions]);

  /** threads open in panes other than the focused one */
  const otherPaneIds = () => threadIds(usePanes.getState().tree, usePanes.getState().focusedKey);

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

  // An app opened from the sidebar or the Apps screen: show it full-width.
  const openedApp = usePinnedApps((s) => s.opened);
  const pinnedApps = usePinnedApps((s) => s.pinned);
  const shownApp = openedApp ? (pinnedApps.find((a) => a.key === openedApp.key) ?? openedApp.app) : undefined;
  useEffect(() => {
    if (openedApp) setView('app-page');
  }, [openedApp]);

  // A plugin opened (sidebar entry, Apps screen, or its own ui.open): show
  // just its page, not the Apps screen.
  const pluginTarget = usePlugins((s) => s.target);
  const targetPlugin = usePlugins((s) => s.plugins.find((p) => p.id === s.target?.id) ?? null);
  useEffect(() => {
    if (pluginTarget) setView('plugin-page');
  }, [pluginTarget]);
  // Loaded up front for the sidebar: plugin entries, and the Apps entry's needs-review count.
  useEffect(() => {
    void usePlugins.getState().reload();
    void useLocalMcp.getState().reload();
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
    // Shown explicitly: the store may reuse the selected thread, which an
    // app pane can have pushed off screen.
    void newSession(otherPaneIds()).then(() => {
      const id = useAppStore.getState().selectedId;
      if (id) usePanes.getState().show(id);
    });
    setView('threads');
  };

  // Dragging from the Apps screen or a full-page app: show the panes to
  // drop on, and go back if the drag is cancelled.
  const dragging = usePanes((s) => !!s.drag);
  const beforeDrag = useRef<{ view: ViewMode; tree: Node } | null>(null);
  useEffect(() => {
    if (dragging) {
      if (view === 'threads') return;
      beforeDrag.current = { view, tree: usePanes.getState().tree };
      setView('threads');
    } else if (beforeDrag.current) {
      const { view: prev, tree: before } = beforeDrag.current;
      beforeDrag.current = null;
      if (usePanes.getState().tree === before) setView(prev);
    }
  }, [dragging]);

  const openGroup = (id: string) => {
    useGroups.getState().open(id);
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
      void newSession(threadIds(p.tree));
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
    toggleSidebar: toggleNav,
    // ⌘S: name the split as a group (an open group already saves itself).
    saveGroup: () => {
      const g = useGroups.getState();
      if (view === 'threads' && !g.activeId && paneList.length > 1) g.setNaming(true);
    },
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
      {/* Rail and sidebar fold away together (⌘B), like Codex: one navigation
          column, and the content gets the whole window. */}
      <div
        inert={!navOpen}
        style={{ width: navOpen ? SIDEBAR_WIDTH + (railShown ? RAIL_WIDTH : 0) : 0 }}
        className="flex shrink-0 overflow-hidden transition-[width] duration-200 ease-out motion-reduce:transition-none"
      >
        <WorkspaceRail />
        <Sidebar
          view={view}
          openIds={openIds}
          onChangeView={setView}
          onOpenThread={openThread}
          onNewThread={newThread}
          onOpenGroup={openGroup}
        />
      </div>
      <SidebarToggle open={navOpen} onToggle={toggleNav} />
      <main className="flex min-w-0 flex-1 flex-col bg-background">
        <MainHeader
          title={
            view === 'app-page'
              ? shownApp?.title
              : view === 'inbox'
              ? 'Inbox'
              : view === 'apps'
              ? 'Apps'
              : view === 'secrets'
              ? 'Secrets'
              : view === 'plugin-page'
              ? targetPlugin?.title
                  : view !== 'threads' || !sole?.content
                  ? undefined
                  : sole.content.kind !== 'thread'
                  ? soleTitle
                  : selected && selected.messages.length > 0
                  ? selected.title
                  : undefined
          }
          maximized={view === 'threads' && maximized !== null}
          navOpen={navOpen}
          group={view === 'threads' && (groupOpen || split) ? <GroupBar canSave={paneList.length > 1} /> : undefined}
        />
        {view === 'threads' && <SplitView overId={overId} />}
        {view === 'inbox' && <InboxView onOpenThread={openThread} />}
        {view === 'app-page' &&
          (shownApp ? (
            <AppView key={shownApp.key} app={shownApp} />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
              This app is gone. Open it from Apps.
            </div>
          ))}
        {view === 'apps' && <AppsPanel connected={connected} />}
        {view === 'secrets' && <SecretsView />}
        {view === 'plugin-page' &&
          (targetPlugin?.enabled && pluginTarget ? (
            <PluginFrame key={targetPlugin.id} plugin={targetPlugin} hash={pluginTarget.hash} />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">This app is off or gone.</div>
          ))}
      </main>
      <DragLayer />
    </div>
  );
}

/**
 * Shows or hides the left column (⌘B). Fixed beside the macOS traffic lights,
 * so it stays put: over the sidebar's drag strip when open, over the main
 * header when closed. Its own element, so a click never starts a window drag.
 */
function SidebarToggle({ open, onToggle }: { open: boolean; onToggle: () => void }) {
  const label = open ? 'Hide sidebar' : 'Show sidebar';
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>
          <button
            onClick={onToggle}
            aria-label={label}
            aria-expanded={open}
            className="fixed top-[3px] left-[76px] z-40 flex size-[22px] items-center justify-center rounded-md text-muted-foreground outline-none transition-colors hover:bg-foreground/[0.07] hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50"
          >
            <PanelLeft className="size-[15px]" />
          </button>
        </TooltipTrigger>
        <TooltipContent side="bottom">
          {label}
          <span className="ml-1.5 opacity-60">⌘B</span>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

/** a pane enlarged from a split (`maximized`): Esc / ⌘↵ goes back */
function MainHeader({
  title,
  maximized,
  navOpen,
  group,
}: {
  title?: string;
  maximized: boolean;
  /** closed: the traffic lights and sidebar toggle sit over this header's left end */
  navOpen: boolean;
  group?: ReactNode;
}) {
  return (
    <div
      data-tauri-drag-region
      className={cn(
        'relative flex h-[52px] shrink-0 items-center justify-center transition-[padding] duration-200',
        navOpen ? 'px-20' : 'px-28',
      )}
    >
      {group || (
        <>
          {title && <span className="truncate text-[13px] font-medium text-foreground/80">{title}</span>}
          {maximized && <span className="ml-2 shrink-0 text-[11px] text-muted-foreground-subtle">Esc to go back</span>}
        </>
      )}
      <AccountMenu />
    </div>
  );
}

/** One-line name field: Enter (or leaving it) saves, Esc cancels. */
function NameInput({
  initial,
  onSubmit,
  onCancel,
  className,
}: {
  initial: string;
  onSubmit: (name: string) => void;
  onCancel: () => void;
  className?: string;
}) {
  const [value, setValue] = useState(initial);
  const done = useRef(false);
  const finish = (save: boolean) => {
    if (done.current) return;
    done.current = true;
    if (save && value.trim()) onSubmit(value.trim());
    else onCancel();
  };
  return (
    <input
      autoFocus
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onFocus={(e) => e.target.select()}
      onClick={(e) => e.stopPropagation()}
      onBlur={() => finish(true)}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === 'Enter') finish(true);
        else if (e.key === 'Escape') {
          e.preventDefault();
          finish(false);
        }
      }}
      className={cn(
        'min-w-0 rounded-md border border-border bg-background px-2 py-0.5 text-[13px] outline-none focus:border-muted-foreground/40',
        className,
      )}
    />
  );
}

/**
 * The split's group: "Save group" (⌘S) once two panes are open, or — while a
 * group is open — its name (click to rename) and "Close group".
 */
function GroupBar({ canSave }: { canSave: boolean }) {
  const active = useGroups((s) => s.groups.find((g) => g.id === s.activeId));
  const naming = useGroups((s) => s.naming);
  const tree = usePanes((s) => s.tree);
  const maximized = usePanes((s) => s.maximized);
  const [renaming, setRenaming] = useState(false);
  const g = useGroups.getState();

  if (active)
    return (
      <>
        <div className="flex min-w-0 items-center gap-1.5">
          <LayoutGrid className="size-3.5 shrink-0 text-muted-foreground" />
          {renaming ? (
            <NameInput
              initial={active.name}
              onSubmit={(name) => {
                g.rename(active.id, name);
                setRenaming(false);
              }}
              onCancel={() => setRenaming(false)}
              className="w-64"
            />
          ) : (
            <button
              onClick={() => setRenaming(true)}
              title="Rename group"
              className="truncate rounded-md px-1.5 py-0.5 text-[13px] font-medium text-foreground/80 hover:bg-accent"
            >
              {active.name}
            </button>
          )}
          {maximized && <span className="shrink-0 text-[11px] text-muted-foreground-subtle">Esc to go back</span>}
        </div>
        <button
          onClick={() => g.close()}
          className="absolute right-12 rounded-md px-2 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
        >
          Close group
        </button>
      </>
    );

  if (!canSave || maximized) return null;
  if (naming)
    return (
      <div className="flex min-w-0 items-center gap-1.5">
        <LayoutGrid className="size-3.5 shrink-0 text-muted-foreground" />
        <NameInput
          initial={defaultGroupName(tree)}
          onSubmit={(name) => g.save(name)}
          onCancel={() => g.setNaming(false)}
          className="w-72"
        />
        <span className="shrink-0 text-[11px] text-muted-foreground-subtle">↵ to save</span>
      </div>
    );
  return (
    <button
      onClick={() => g.setNaming(true)}
      className="absolute right-12 flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
    >
      <LayoutGrid className="size-3.5" />
      Save group
      <kbd className="text-[11px] text-muted-foreground-subtle">⌘S</kbd>
    </button>
  );
}

/**
 * Window-level shortcuts: ⌘T new thread in the focused pane, ⌘\ split it
 * with a new thread, ⌘W close the focused pane, ⌘1–⌘4 focus pane N, ⌘[ / ⌘]
 * previous/next pane, ⌘↵ maximize/restore the focused pane, ⌘S save the
 * split as a group, ⌘B show/hide the sidebar, Esc restore,
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
  saveGroup: () => void;
  toggleSidebar: () => void;
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
      } else if (e.metaKey && !e.shiftKey && e.key.toLowerCase() === 's') {
        e.preventDefault();
        ref.current.saveGroup();
      } else if (e.metaKey && !e.shiftKey && !e.altKey && e.key.toLowerCase() === 'b') {
        // ⌘ only: ⌃B moves the caret back in macOS text fields.
        e.preventDefault();
        ref.current.toggleSidebar();
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

function WorkspaceAvatar({ workspace: w }: { workspace: WorkspaceInfo }) {
  const [broken, setBroken] = useState<string | null>(null);
  const logo = w.logoUrl && w.logoUrl !== broken ? w.logoUrl : null;
  return (
    <span
      style={logo ? undefined : { backgroundColor: `hsl(${hue(w.id)} 38% 50%)` }}
      className={cn(
        'flex size-10 items-center justify-center overflow-hidden rounded-[12px] text-[13px] font-semibold text-white transition-transform group-active:scale-95',
        logo && 'bg-white ring-1 ring-black/10 ring-inset',
      )}
    >
      {logo ? <img src={logo} alt="" className="size-full object-cover" onError={() => setBroken(logo)} /> : initials(w.name)}
    </span>
  );
}

function WorkspaceRail() {
  const workspaces = useAppStore((s) => s.workspaces);
  const workspaceId = useAppStore((s) => s.workspaceId);
  const setWorkspace = useAppStore((s) => s.setWorkspace);
  // One workspace needs no switcher.
  if (workspaces.length < 2) return null;

  return (
    <nav style={{ width: RAIL_WIDTH }} className="relative flex shrink-0 flex-col items-center gap-2.5 bg-sidebar pb-3">
      <div data-tauri-drag-region className="h-[52px] w-full shrink-0" />
      <span aria-hidden className="pointer-events-none absolute top-[52px] right-0 bottom-0 w-px bg-border" />
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
                      'absolute -left-4 w-1 rounded-r-full bg-foreground transition-all',
                      active ? 'h-7' : 'h-0 group-hover:h-3',
                    )}
                  />
                  <WorkspaceAvatar workspace={w} />
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
  onOpenGroup,
}: {
  view: ViewMode;
  /** threads open in a pane */
  openIds: Set<string>;
  onChangeView: (v: ViewMode) => void;
  onOpenThread: (id: string) => void;
  onNewThread: () => void;
  onOpenGroup: (id: string) => void;
}) {
  const sessions = useAppStore((s) => s.sessions);
  const selectedId = useAppStore((s) => s.selectedId);
  const running = useAppStore((s) => s.running);
  const inThreads = view === 'threads';
  const cloudInbox = useAppStore((s) => s.cloudInbox);
  const approvals = useAppStore((s) => s.approvals);
  const appsAttention = useAppsAttention();
  const inboxCount = useMemo(
    () => localInbox(sessions, running, approvals).length + cloudInbox.filter((i) => i.status !== 'completed').length,
    [sessions, running, approvals, cloudInbox],
  );

  // The middle's sections, in the user's order: sections with nothing to show
  // keep their saved place for when they come back.
  const hasPinned = usePinnedApps((s) => s.pinned.length > 0);
  const hasGroups = useGroups((s) => s.groups.length > 0);
  const plugins = usePlugins((s) => s.plugins);
  const pluginTarget = usePlugins((s) => s.target?.id);
  const hidden = useSidebarLayout((s) => s.hidden);
  const savedOrder = useSidebarLayout((s) => s.order);
  const pluginById = useMemo(
    () => new Map(plugins.filter((p) => p.enabled).map((p) => [pluginSection(p.id), p])),
    [plugins],
  );
  const order = useMemo(() => {
    const available = [
      ...(hasPinned ? ['pinned'] : []),
      ...[...pluginById.keys()].filter((id) => !hidden.includes(id)),
      ...(hasGroups ? ['groups'] : []),
      'threads',
    ];
    return fillOrder(savedOrder.filter((id) => available.includes(id)), available);
  }, [hasPinned, hasGroups, pluginById, hidden, savedOrder]);

  return (
    <aside data-sidebar style={{ width: SIDEBAR_WIDTH }} className="flex shrink-0 flex-col bg-sidebar text-sidebar-foreground">
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
        <SidebarNavItem
          icon={AppWindow}
          label="Apps"
          count={appsAttention}
          attention
          active={view === 'apps'}
          onClick={() => {
            useAppsScreen.getState().show(null);
            onChangeView('apps');
          }}
        />
        <SidebarNavItem icon={KeyRound} label="Secrets" active={view === 'secrets'} onClick={() => onChangeView('secrets')} />
      </div>

      {/* One scroll area for every section, so an open plugin panel never hides
          Threads. Radix wraps content in a display:table div that grows with
          the longest title; make it a block so titles truncate and row badges
          stay visible. */}
      <ScrollArea className="min-h-0 flex-1" viewportClassName="[&>div]:!block">
        <SectionOrder.Provider value={order}>
          <div className="flex flex-col px-2.5 pb-2">
            {order.map((id, i) => (
              <SidebarSection key={id} id={id} gap={i === 0 || !(isPluginSection(id) && isPluginSection(order[i - 1]))}>
                {id === 'pinned' ? (
                  <PinnedAppsSection active={view === 'app-page'} />
                ) : id === 'groups' ? (
                  <GroupsSection active={inThreads} onOpen={onOpenGroup} />
                ) : id === 'threads' ? (
                  <ThreadsSection
                    inThreads={inThreads}
                    openIds={openIds}
                    selectedId={selectedId}
                    onOpenThread={onOpenThread}
                  />
                ) : (
                  <PluginEntry
                    plugin={pluginById.get(id)!}
                    active={view === 'plugin-page' && pluginTarget === pluginById.get(id)!.id}
                  />
                )}
              </SidebarSection>
            ))}
          </div>
        </SectionOrder.Provider>
      </ScrollArea>
      <SidebarDragLayer />
    </aside>
  );
}

/** The threads, newest first; drag one onto the main area to open it in a split. */
function ThreadsSection({
  inThreads,
  openIds,
  selectedId,
  onOpenThread,
}: {
  inThreads: boolean;
  openIds: Set<string>;
  selectedId: string | null;
  onOpenThread: (id: string) => void;
}) {
  const sessions = useAppStore((s) => s.sessions);
  const running = useAppStore((s) => s.running);
  const approvals = useAppStore((s) => s.approvals);
  const deleteSession = useAppStore((s) => s.deleteSession);
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, 'threads'));
  return (
    <>
      <SectionHeader id="threads" label="Threads" />
      {!collapsed && (
        <div className="flex flex-col gap-px">
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
      )}
    </>
  );
}

/** Saved splits: click to restore one exactly; rename or delete on hover. */
function GroupsSection({ active, onOpen }: { active: boolean; onOpen: (id: string) => void }) {
  const groups = useGroups((s) => s.groups);
  const activeId = useGroups((s) => s.activeId);
  const [editing, setEditing] = useState<string | null>(null);
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, 'groups'));
  if (!groups.length) return null;
  return (
    <>
      <SectionHeader id="groups" label="Groups" />
      {!collapsed && (
        <div className="flex flex-col gap-px">
          {groups.map((g) => {
            const n = leaves(g.tree).length;
            return (
              <div
                key={g.id}
                onClick={() => editing !== g.id && onOpen(g.id)}
                title={`${g.name} — ${n} pane${n === 1 ? '' : 's'}`}
                className={cn(
                  'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[7px] transition-colors select-none',
                  active && activeId === g.id ? 'bg-sidebar-accent' : 'hover:bg-sidebar-hover',
                )}
              >
                <LayoutGrid className="size-3.5 shrink-0 text-muted-foreground" />
                {editing === g.id ? (
                  <NameInput
                    initial={g.name}
                    onSubmit={(name) => {
                      useGroups.getState().rename(g.id, name);
                      setEditing(null);
                    }}
                    onCancel={() => setEditing(null)}
                    className="flex-1 py-0 text-[12.5px]"
                  />
                ) : (
                  <span className="min-w-0 flex-1 truncate text-[13px] leading-tight">{g.name}</span>
                )}
                <span className="shrink-0 text-[11px] text-muted-foreground-subtle tabular-nums group-hover:hidden">{n}</span>
                {editing !== g.id && (
                  <>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        setEditing(g.id);
                      }}
                      className="hidden shrink-0 text-muted-foreground hover:text-foreground group-hover:block"
                      aria-label={`Rename ${g.name}`}
                    >
                      <Pencil className="size-3" />
                    </button>
                    <button
                      onClick={(e) => {
                        e.stopPropagation();
                        useGroups.getState().remove(g.id);
                      }}
                      className="hidden shrink-0 text-muted-foreground hover:text-foreground group-hover:block"
                      aria-label={`Delete ${g.name}`}
                    >
                      <X className="size-3.5" />
                    </button>
                  </>
                )}
              </div>
            );
          })}
        </div>
      )}
    </>
  );
}

/** Apps pinned from the Apps screen, one click away. */
function PinnedAppsSection({ active }: { active: boolean }) {
  const pinned = usePinnedApps((s) => s.pinned);
  const opened = usePinnedApps((s) => s.opened);
  const open = usePinnedApps((s) => s.open);
  const unpin = usePinnedApps((s) => s.unpin);
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, 'pinned'));
  if (!pinned.length) return null;
  return (
    <>
      <SectionHeader id="pinned" label="Pinned" />
      {!collapsed &&
        pinned.map((a) => (
          <div
            key={a.key}
            onClick={() => open(a.key, a)}
            // Drag onto the main area to open it in a split.
            onPointerDown={(e) => startPaneDrag(e, { kind: 'app', app: a })}
            title={`${a.title} — ${a.kind === 'local' ? 'on this device' : 'AgentArea'} · ${a.subtitle}`}
            className={cn(
              'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[6px] text-[13px] transition-colors select-none',
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
    </>
  );
}

function SidebarNavItem({
  icon: Icon,
  label,
  count,
  attention,
  active,
  onClick,
}: {
  icon: typeof AppWindow;
  label: string;
  /** badge, e.g. items waiting in the inbox */
  count?: number;
  /** the badge means something needs review, not unread items */
  attention?: boolean;
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
      {!!count &&
        (attention ? (
          <span
            title={`${count} need${count === 1 ? 's' : ''} review`}
            className="ml-auto rounded-full bg-amber-500/15 px-1.5 text-[10.5px] font-medium text-amber-700 dark:text-amber-500"
          >
            {count}
          </span>
        ) : (
          <span className="ml-auto rounded-full bg-foreground px-1.5 text-[10.5px] font-semibold text-background">{count}</span>
        ))}
    </button>
  );
}

const CONNECTION = {
  connected: { dot: 'bg-status-success', label: 'Connected' },
  connecting: { dot: 'bg-chart-2', label: 'Connecting…' },
  offline: { dot: 'bg-destructive', label: 'Offline' },
} as const;

/** "ada.lovelace@x.io" → "AL", "you@x.io" → "Y" */
function userInitials(name: string | undefined, email: string | undefined): string {
  const parts = name?.trim()
    ? name.trim().split(/\s+/)
    : (email ?? '').split('@')[0].split(/[._+-]+/).filter(Boolean);
  return ((parts[0]?.[0] ?? '?') + (parts[1]?.[0] ?? '')).toUpperCase();
}

/**
 * The account, top right: theme, environment and connection, sign out. The
 * connection shows on the avatar only when something is wrong.
 */
function AccountMenu() {
  const theme = useAppStore((s) => s.settings.theme);
  const toggleTheme = useAppStore((s) => s.toggleTheme);
  const logout = useAppStore((s) => s.logout);
  const email = useAppStore((s) => s.auth.user?.email);
  const name = useAppStore((s) => s.auth.user?.name);
  const status = useAppStore((s) => s.connection);
  const connectionError = useAppStore((s) => s.connectionError);
  const connect = useAppStore((s) => s.connect);
  const envLabel = useAppStore((s) => s.env.label);
  const conn = CONNECTION[status];
  const signedIn = Boolean(email);
  const problem = signedIn && status !== 'connected';
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          aria-label={problem ? `Account — ${conn.label}` : 'Account'}
          title={problem ? `${envLabel} · ${conn.label}` : (name ?? email ?? 'Not signed in')}
          className="absolute right-3 flex size-7 items-center justify-center rounded-full bg-foreground/[0.07] text-[11px] font-semibold text-foreground/70 outline-none transition-colors hover:bg-foreground/[0.11] focus-visible:ring-2 focus-visible:ring-ring/50 hover:text-foreground data-[state=open]:bg-foreground/[0.13]"
        >
          {userInitials(name, email)}
          {problem && (
            <span className={cn('absolute -right-px -bottom-px size-2.5 rounded-full ring-2 ring-background', conn.dot)} />
          )}
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" sideOffset={6} className="w-60">
        <div className="flex flex-col gap-1 px-2 py-1.5">
          {signedIn ? (
            <>
              <span className="truncate text-[13px] font-medium text-foreground">{name ?? email}</span>
              {name && <span className="truncate text-[11.5px] text-muted-foreground">{email}</span>}
              <span className="flex min-w-0 items-center gap-1.5 text-[11.5px] text-muted-foreground">
                <span className={cn('size-1.5 shrink-0 rounded-full', conn.dot, status === 'connecting' && 'animate-pulse')} />
                <span className="truncate">
                  {envLabel} · {conn.label}
                </span>
              </span>
              {status === 'offline' && connectionError && (
                <span className="text-[11px] break-words text-destructive">{connectionError}</span>
              )}
              {status === 'offline' && (
                <button
                  className="self-start text-[11.5px] text-muted-foreground underline underline-offset-2 hover:text-foreground"
                  onClick={() => void connect()}
                >
                  Reconnect
                </button>
              )}
            </>
          ) : (
            <span className="text-[13px] font-medium text-foreground">Not signed in</span>
          )}
        </div>
        <DropdownMenuSeparator />
        {/* Stays open: the flip is the feedback. */}
        <DropdownMenuItem
          onSelect={(e) => {
            e.preventDefault();
            toggleTheme();
          }}
        >
          {theme === 'dark' ? <Sun /> : <Moon />}
          {theme === 'dark' ? 'Light mode' : 'Dark mode'}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={logout}>
          {signedIn ? <LogOut /> : <LogIn />}
          {signedIn ? 'Sign out' : 'Sign in'}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
