import { Fragment, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type RefObject } from 'react';
import { ArrowUp, Plus, Puzzle, Square, X } from 'lucide-react';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';
import type { Session } from '@/data';
import { RunnerBadge } from '@/assets/icons';
import {
  contentId,
  leaves,
  MAX_PANES,
  thread,
  threadIds,
  usePanes,
  zoneAt,
  type DropTarget,
  type Leaf,
  type Node,
  type PaneContent,
  type Split,
} from '@/lib/panes';
import type { AppRef } from '@/lib/pinnedApps';
import { useLocalMcp } from '@/lib/localMcp';
import { usePlugins } from '@/lib/plugins';
import { ScrollArea } from '@/components/ui/scroll-area';
import { AppView } from './AppView';
import { EntityIcon } from './EntityIcon';
import { PluginIcon } from './PluginIcon';
import { PluginFrame } from './PluginFrame';
import { basename } from './format';
import { ThreadFolder } from './FileChip';
import { ApprovalCard, DraftRow, groupMessages, MessageRow, ToolGroup } from './MessageView';
import { AttachmentChips, Composer } from './Composer';
import { ContextPanel, ContextToggle, useContextPanel } from './ContextPanel';
import { DropOverlay } from './DropZone';

// Enough context to reply well from a pane, without rendering a whole thread's history.
const TILE_TRANSCRIPT_SIZE = 14;
/** pixels the pointer moves before a press on a thread becomes a drag */
export const DRAG_THRESHOLD = 4;
/** narrowest a pane gets when resizing, in pixels */
const MIN_PANE = 220;

/* ── Dragging a thread, app or plugin page (sidebar, Apps screen, pane header) onto the panes ───── */

/** Where an item dragged to (x, y) would land, if anywhere. */
export function targetAt(item: PaneContent, x: number, y: number): DropTarget | null {
  const list = leaves(usePanes.getState().tree);
  const from = list.find((l) => contentId(l.content) === contentId(item));
  for (const el of document.querySelectorAll<HTMLElement>('[data-pane-key]')) {
    const r = el.getBoundingClientRect();
    if (x < r.left || x > r.right || y < r.top || y > r.bottom) continue;
    const paneKey = el.dataset.paneKey!;
    // Dropping a pane on itself changes nothing.
    if (from?.key === paneKey) return null;
    // Moving a pane never adds one, so it can always go beside another.
    const zone = zoneAt(r, x, y, { edges: !!from || list.length < MAX_PANES, vertical: list.length > 1 });
    const pad = 6;
    const w = r.width - pad * 2;
    const h = r.height - pad * 2;
    const rect = { left: r.left + pad, top: r.top + pad, width: w, height: h };
    if (zone === 'left' || zone === 'right') rect.width = w / 2 - pad / 2;
    if (zone === 'right') rect.left += w / 2 + pad / 2;
    if (zone === 'top' || zone === 'bottom') rect.height = h / 2 - pad / 2;
    if (zone === 'bottom') rect.top += h / 2 + pad / 2;
    return { paneKey, zone, rect };
  }
  return null;
}

export const swallowClick = (e: MouseEvent) => {
  e.stopPropagation();
  e.preventDefault();
};

/**
 * Start tracking a press on a thread, app or plugin entry. Under
 * DRAG_THRESHOLD it stays a click; past it, it becomes a drag with our own
 * ghost and drop overlay (pointer events, not HTML5 drag & drop, which Tauri's
 * native file drop can swallow). Esc cancels.
 */
export function startPaneDrag(e: ReactPointerEvent, item: PaneContent) {
  // Controls inside the source keep their clicks; the source itself may be a button.
  const hit = (e.target as HTMLElement).closest('button, textarea, input, a');
  if (e.button !== 0 || (hit && hit !== e.currentTarget)) return;
  const x0 = e.clientX;
  const y0 = e.clientY;
  let dragging = false;

  const move = (ev: PointerEvent) => {
    if (!dragging) {
      if (Math.hypot(ev.clientX - x0, ev.clientY - y0) < DRAG_THRESHOLD) return;
      dragging = true;
      document.body.classList.add('pane-dragging');
    }
    usePanes.getState().setDrag({ item, x: ev.clientX, y: ev.clientY, target: targetAt(item, ev.clientX, ev.clientY) });
  };
  const end = (drop: boolean) => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    window.removeEventListener('pointercancel', cancel);
    window.removeEventListener('keydown', key, true);
    if (!dragging) return;
    document.body.classList.remove('pane-dragging');
    const target = usePanes.getState().drag?.target;
    usePanes.getState().setDrag(null);
    if (drop && target) usePanes.getState().drop(item, target);
    // The release that ends a drag must not also click what it lands on.
    window.addEventListener('click', swallowClick, { capture: true, once: true });
    setTimeout(() => window.removeEventListener('click', swallowClick, true), 0);
  };
  const up = () => end(true);
  const cancel = () => end(false);
  const key = (ev: KeyboardEvent) => {
    if (ev.key !== 'Escape' || !dragging) return;
    ev.preventDefault();
    ev.stopPropagation();
    end(false);
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
  window.addEventListener('pointercancel', cancel);
  window.addEventListener('keydown', key, true);
}

export const startThreadDrag = (e: ReactPointerEvent, sessionId: string) => startPaneDrag(e, thread(sessionId));

/** What a pane's content is called: the thread's, app's or plugin's title. */
export function useContentTitle(item: PaneContent | null): string {
  const sessionTitle = useAppStore((s) => (item?.kind === 'thread' ? s.sessions.find((x) => x.id === item.id)?.title : undefined));
  const pluginTitle = usePlugins((s) => (item?.kind === 'plugin' ? s.plugins.find((p) => p.id === item.id)?.title : undefined));
  if (item?.kind === 'app') return item.app.title;
  if (item?.kind === 'plugin') return pluginTitle ?? item.id;
  return sessionTitle ?? 'Thread';
}

/** The small glyph a pane's content goes by: its runner, the app's logo, or a plugin's piece. */
export function ContentIcon({ item }: { item: PaneContent }) {
  const runner = useAppStore((s) => (item.kind === 'thread' ? s.sessions.find((x) => x.id === item.id)?.runner : undefined));
  const plugin = usePlugins((s) => (item.kind === 'plugin' ? s.plugins.find((p) => p.id === item.id) : undefined));
  if (item.kind === 'app')
    return <EntityIcon name={item.app.title} icons={item.app.icons} seed={item.app.seed} className="size-3.5 shrink-0 rounded-[4px] text-[6px]" />;
  if (item.kind === 'plugin') return plugin ? <PluginIcon plugin={plugin} className="size-3.5 text-muted-foreground" /> : <Puzzle className="size-3.5 shrink-0 text-muted-foreground" />;
  return runner ? <RunnerBadge runner={runner} className="size-3.5 shrink-0 text-muted-foreground" /> : null;
}

/**
 * The ghost under the cursor and the overlay where the item would land, over
 * a full-window capture layer while dragging or resizing: iframes (apps,
 * plugins) would otherwise swallow the pointer as it crosses them.
 */
export function DragLayer() {
  const drag = usePanes((s) => s.drag);
  const resizing = usePanes((s) => s.resizing);
  const title = useContentTitle(drag?.item ?? null);
  // Keep the last spot so the overlay fades out in place instead of vanishing.
  const last = useRef<DropTarget['rect'] | null>(null);
  if (!drag) {
    last.current = null;
    return resizing ? <div className="fixed inset-0 z-30" /> : null;
  }
  if (drag.target) last.current = drag.target.rect;
  const rect = last.current;

  return (
    <>
      <div className="fixed inset-0 z-30" />
      {rect && (
        <div
          className="pointer-events-none fixed z-40 animate-in rounded-xl border-[1.5px] border-drop bg-drop/[0.12] transition-all duration-[120ms] ease-out fade-in-0"
          style={{ ...rect, opacity: drag.target ? 1 : 0 }}
        />
      )}
      <div
        className="pointer-events-none fixed z-50 flex max-w-[240px] items-center gap-1.5 rounded-lg border border-border bg-popover/95 px-2.5 py-1.5 text-[12px] font-medium text-foreground shadow-[0_6px_20px_rgba(0,0,0,0.12)] backdrop-blur-sm"
        // Near the right edge the ghost flips to the cursor's left so it stays in view.
        style={
          drag.x > window.innerWidth - 260
            ? { left: drag.x - 14, top: drag.y + 12, transform: 'translateX(-100%)' }
            : { left: drag.x + 14, top: drag.y + 12 }
        }
      >
        <ContentIcon item={drag.item} />
        <span className="truncate">{title}</span>
      </div>
    </>
  );
}

/* ── The panes ─────────────────────────────────────────── */

/** What is on screen: one thread, app or plugin page filling the main area, or up to four split. */
export function SplitView({ overId }: { overId: string | null }) {
  const tree = usePanes((s) => s.tree);
  const maximized = usePanes((s) => s.maximized);
  const one = tree.kind === 'leaf' ? tree : maximized ? leaves(tree).find((l) => l.key === maximized) : undefined;
  const c = one?.content;
  const session = useAppStore((s) => (c?.kind === 'thread' ? s.sessions.find((x) => x.id === c.id) : undefined));

  // A click into an app's or plugin's iframe never reaches this document;
  // the window losing focus to it is the only sign — focus that pane.
  useEffect(() => {
    const onBlur = () =>
      setTimeout(() => {
        const el = document.activeElement;
        if (el?.tagName !== 'IFRAME') return;
        const key = el.closest<HTMLElement>('[data-pane-key]')?.dataset.paneKey;
        if (key && usePanes.getState().focusedKey !== key) usePanes.getState().focus(key);
      }, 0);
    window.addEventListener('blur', onBlur);
    return () => window.removeEventListener('blur', onBlur);
  }, []);

  if (one)
    return (
      <div data-pane-key={one.key} data-session-id={session?.id} className="relative flex min-h-0 flex-1 flex-col">
        {c?.kind === 'thread' ? (
          session ? (
            <SessionBody session={session} overId={overId} />
          ) : (
            <Dangling leaf={one} text="Thread was deleted" />
          )
        ) : c?.kind === 'app' ? (
          <AppPane leaf={one} app={c.app} full />
        ) : c?.kind === 'plugin' ? (
          <PluginPane leaf={one} id={c.id} full />
        ) : null}
      </div>
    );

  return (
    <div className="flex min-h-0 flex-1 px-3 pb-3">
      <NodeView node={tree} path={[]} overId={overId} />
    </div>
  );
}

function NodeView({ node, path, overId }: { node: Node; path: number[]; overId: string | null }) {
  return node.kind === 'leaf' ? <Pane leaf={node} overId={overId} /> : <SplitBox split={node} path={path} overId={overId} />;
}

function SplitBox({ split, path, overId }: { split: Split; path: number[]; overId: string | null }) {
  const box = useRef<HTMLDivElement>(null);
  const row = split.dir === 'row';
  return (
    <div ref={box} className={cn('flex min-h-0 min-w-0 flex-1', row ? 'flex-row' : 'flex-col')}>
      {split.children.map((c, i) => (
        <Fragment key={leaves(c)[0].key}>
          {i > 0 && <Divider row={row} path={path} index={i} sizes={split.sizes} box={box} />}
          <div className="flex min-h-0 min-w-0" style={{ flex: `${split.sizes[i]} 1 0px` }}>
            <NodeView node={c} path={[...path, i]} overId={overId} />
          </div>
        </Fragment>
      ))}
    </div>
  );
}

/** Drag to resize the two panes beside it; double-click to make them all equal. */
function Divider({
  row,
  path,
  index,
  sizes,
  box,
}: {
  row: boolean;
  path: number[];
  /** the child after this divider */
  index: number;
  sizes: number[];
  box: RefObject<HTMLDivElement | null>;
}) {
  const setSizes = usePanes((s) => s.setSizes);
  const resetSizes = usePanes((s) => s.resetSizes);
  const setResizing = usePanes((s) => s.setResizing);
  const [active, setActive] = useState(false);

  const onPointerDown = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 || !box.current) return;
    e.preventDefault();
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    const r = box.current.getBoundingClientRect();
    const total = row ? r.width : r.height;
    const start = row ? e.clientX : e.clientY;
    const a = sizes[index - 1];
    const pair = a + sizes[index];
    const min = Math.min(MIN_PANE / total, pair / 2);
    let next = sizes;
    setActive(true);
    setResizing(row ? 'row' : 'col');
    document.body.classList.add(row ? 'pane-resizing-row' : 'pane-resizing-col');

    const move = (ev: PointerEvent) => {
      const d = ((row ? ev.clientX : ev.clientY) - start) / total;
      const na = Math.min(Math.max(a + d, min), pair - min);
      next = sizes.map((s, j) => (j === index - 1 ? na : j === index ? pair - na : s));
      setSizes(path, next);
    };
    const up = () => {
      el.removeEventListener('pointermove', move);
      el.removeEventListener('pointerup', up);
      el.removeEventListener('pointercancel', up);
      document.body.classList.remove('pane-resizing-row', 'pane-resizing-col');
      setActive(false);
      setResizing(null);
      setSizes(path, next, true);
    };
    el.addEventListener('pointermove', move);
    el.addEventListener('pointerup', up);
    el.addEventListener('pointercancel', up);
  };

  return (
    <div
      role="separator"
      aria-orientation={row ? 'vertical' : 'horizontal'}
      title="Drag to resize · double-click to reset"
      onPointerDown={onPointerDown}
      onDoubleClick={() => resetSizes(path)}
      className={cn('group flex shrink-0 items-center justify-center', row ? 'w-2 cursor-col-resize' : 'h-2 cursor-row-resize')}
    >
      <span
        className={cn(
          'rounded-full transition-colors duration-150',
          row ? 'h-full w-px' : 'h-px w-full',
          active ? 'bg-drop' : 'bg-transparent group-hover:bg-foreground/15',
        )}
      />
    </div>
  );
}

function Pane({ leaf, overId }: { leaf: Leaf; overId: string | null }) {
  const c = leaf.content;
  const session = useAppStore((s) => (c?.kind === 'thread' ? s.sessions.find((x) => x.id === c.id) : undefined));
  const focused = usePanes((s) => s.focusedKey === leaf.key);
  const focus = usePanes((s) => s.focus);
  const moving = usePanes((s) => !!c && !!s.drag && contentId(s.drag.item) === contentId(c));

  return (
    <div
      data-pane-key={leaf.key}
      data-session-id={session?.id}
      // Clicking or typing into a pane makes it the one shortcuts act on.
      onFocusCapture={focused ? undefined : () => focus(leaf.key)}
      onMouseDown={focused ? undefined : () => focus(leaf.key)}
      className={cn(
        'relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded-xl border bg-card transition-[border-color,box-shadow,opacity] duration-150',
        focused ? 'border-foreground/20 shadow-[0_0_0_3px_rgba(0,0,0,0.035)] dark:shadow-[0_0_0_3px_rgba(255,255,255,0.05)]' : 'border-border',
        moving && 'opacity-50',
      )}
    >
      {c?.kind === 'thread' ? (
        session ? (
          <PaneBody leaf={leaf} session={session} focused={focused} />
        ) : (
          <Dangling leaf={leaf} text="Thread was deleted" />
        )
      ) : c?.kind === 'app' ? (
        <AppPane leaf={leaf} app={c.app} />
      ) : c?.kind === 'plugin' ? (
        <PluginPane leaf={leaf} id={c.id} />
      ) : (
        <EmptyPane leaf={leaf} />
      )}
      {session && overId === session.id && <DropOverlay />}
    </div>
  );
}

/** A new thread in pane `key` — shown there even when the store reuses the selected one. */
async function newThreadIn(key: string) {
  usePanes.getState().focus(key);
  await useAppStore.getState().newSession(threadIds(usePanes.getState().tree));
  const id = useAppStore.getState().selectedId;
  if (id) usePanes.getState().show(id);
}

/** A pane whose thread, app or plugin is gone (a saved group outlives them). */
function Dangling({ leaf, text }: { leaf: Leaf; text: string }) {
  const remove = () => {
    const p = usePanes.getState();
    if (leaves(p.tree).length > 1) return p.close(leaf.key);
    p.clear(leaf.key);
    void newThreadIn(leaf.key);
  };
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-2.5 p-6 text-center">
      <span className="text-[13px] text-muted-foreground">{text}</span>
      <button
        onClick={remove}
        className="rounded-md border border-border px-2.5 py-1 text-[12px] text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
      >
        Remove
      </button>
    </div>
  );
}

/** Pane chrome for anything on screen: drag to move, double-click to maximize, × to close. */
function PaneHeader({ leaf, title, children }: { leaf: Leaf; title: string; children?: React.ReactNode }) {
  const close = usePanes((s) => s.close);
  const toggleMaximize = usePanes((s) => s.toggleMaximize);
  return (
    <div
      onPointerDown={(e) => leaf.content && startPaneDrag(e, leaf.content)}
      onDoubleClick={(e) => {
        if ((e.target as HTMLElement).closest('button')) return;
        usePanes.getState().focus(leaf.key);
        toggleMaximize();
      }}
      title="Drag to move · double-click to maximize (⌘↵)"
      className="@container flex h-9 shrink-0 cursor-grab items-center gap-2 border-b border-border pr-1.5 pl-3 select-none"
    >
      {leaf.content && <ContentIcon item={leaf.content} />}
      <span className="min-w-0 truncate text-[12.5px] font-medium text-foreground/90">{title}</span>
      {children}
      <button
        onClick={() => close(leaf.key)}
        aria-label="Close pane"
        title="Close pane (⌘W)"
        className="flex size-6 shrink-0 items-center justify-center rounded-md text-muted-foreground-subtle transition-colors hover:bg-accent hover:text-foreground"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

/** An MCP App in a pane: compact header here, the app's own below (or its full view when alone). */
function AppPane({ leaf, app, full }: { leaf: Leaf; app: AppRef; full?: boolean }) {
  // Known only once the local server list has loaded; otherwise AppView reports it.
  const missing = useLocalMcp(
    (s) => app.kind === 'local' && !s.listError && s.servers.length > 0 && !s.servers.some((x) => x.id === app.serverId),
  );
  if (missing) return <Dangling leaf={leaf} text="App no longer installed" />;
  // Views the app opens through app links go in the pane to its right.
  const openLinked = (linked: AppRef) => usePanes.getState().openLinked(leaf.key, linked);
  if (full) return <AppView key={app.key} app={app} onOpenLinked={openLinked} />;
  return (
    <>
      <PaneHeader leaf={leaf} title={app.title}>
        <span className="flex-1" />
        <span className="shrink-0 rounded-full border border-border px-1.5 py-px text-[10px] text-muted-foreground">
          {app.kind === 'local' ? 'Local' : 'Cloud'}
        </span>
      </PaneHeader>
      <AppView key={app.key} app={app} chrome={false} onOpenLinked={openLinked} />
    </>
  );
}

/** A plugin's main page in a pane. */
function PluginPane({ leaf, id, full }: { leaf: Leaf; id: string; full?: boolean }) {
  const plugin = usePlugins((s) => s.plugins.find((p) => p.id === id));
  if (!plugin?.enabled) return <Dangling leaf={leaf} text={plugin ? 'App is turned off' : 'App no longer installed'} />;
  return (
    <>
      {!full && (
        <PaneHeader leaf={leaf} title={plugin.title}>
          <span className="flex-1" />
        </PaneHeader>
      )}
      <PluginFrame key={plugin.id} plugin={plugin} />
    </>
  );
}

/** A pane opened with no thread yet (e.g. ⌘\ before the thread exists). */
function EmptyPane({ leaf }: { leaf: Leaf }) {
  const close = usePanes((s) => s.close);
  const newThread = () => void newThreadIn(leaf.key);
  return (
    <div className="relative flex flex-1 flex-col items-center justify-center gap-2">
      <button
        onClick={newThread}
        className="flex flex-col items-center gap-2 rounded-xl px-6 py-4 text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
      >
        <Plus className="size-5" />
        <span className="text-[13px] font-medium">New thread</span>
      </button>
      <button
        onClick={() => close(leaf.key)}
        aria-label="Close pane"
        className="absolute top-1.5 right-1.5 flex size-6 items-center justify-center rounded-md text-muted-foreground-subtle hover:bg-accent hover:text-foreground"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

function PaneBody({ leaf, session, focused }: { leaf: Leaf; session: Session; focused: boolean }) {
  const running = useAppStore((s) => !!s.running[session.id]);
  const streaming = useAppStore((s) => s.drafts[session.id] ?? '');
  const approvals = useAppStore((s) => s.approvals[session.id]);
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: 'end' });
  }, [session.messages.length, running, streaming, approvals?.length]);

  const last = session.messages[session.messages.length - 1];
  const needsReply = !running && last?.role === 'assistant';
  const recent = session.messages.slice(-TILE_TRANSCRIPT_SIZE);
  const fresh = session.messages.length === 0;
  const folderLabel = session.runner === 'cloud' ? null : session.cwd ? basename(session.cwd) : null;
  const [ctx, setCtx] = useState(false);

  return (
    <>
      {ctx && <ContextPanel session={session} onClose={() => setCtx(false)} className="absolute top-9 right-0 bottom-0 z-20 shadow-[-8px_0_24px_rgba(0,0,0,0.08)]" />}
      <PaneHeader leaf={leaf} title={session.title}>
        {folderLabel && (
          <span className="hidden min-w-0 shrink-[2] truncate text-[11px] text-muted-foreground-subtle @md:inline">{folderLabel}</span>
        )}
        <span className="flex-1" />
        {approvals?.length ? (
          <span className="shrink-0 rounded-full bg-amber-500/10 px-1.5 py-0.5 text-[10px] font-medium text-amber-600">
            Needs approval
          </span>
        ) : running ? (
          <span className="flex shrink-0 items-center gap-1.5 text-[11px] text-muted-foreground">
            <span className="size-1.5 animate-pulse rounded-full bg-chart-2" />
            Running
          </span>
        ) : needsReply ? (
          <span className="shrink-0 rounded-full bg-status-success-background px-1.5 py-0.5 text-[10px] font-medium text-status-success">
            Needs reply
          </span>
        ) : null}
        <ContextToggle session={session} open={ctx} onToggle={() => setCtx((v) => !v)} className="h-6 shrink-0 px-1.5" />
      </PaneHeader>

      {fresh ? (
        // A new thread gets the full composer: runner, folder and tools are picked here.
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center overflow-y-auto scrollbar-sleek px-4 pb-8">
          <h2 className="mb-4 text-[17px] font-semibold tracking-tight text-foreground">What should we work on?</h2>
          <div className="w-full max-w-[560px]">
            <Composer session={session} autoFocus={focused} />
          </div>
        </div>
      ) : (
        <>
          <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek px-3.5 py-2.5">
            <div className="flex flex-col gap-2.5">
              <ThreadFolder session={session}>
                {groupMessages(recent).map((g) =>
                  Array.isArray(g) ? <ToolGroup key={g[0].id} tools={g} dense /> : <MessageRow key={g.id} message={g} dense />,
                )}
                {streaming && <DraftRow text={streaming} dense />}
              </ThreadFolder>
              {approvals?.map((a) => <ApprovalCard key={a.id} approval={a} dense />)}
              <div ref={bottomRef} />
            </div>
          </div>

          <div className="shrink-0 border-t border-border p-2">
            <AttachmentChips session={session} />
            <TileComposer session={session} />
          </div>
        </>
      )}
    </>
  );
}

function TileComposer({ session }: { session: Session }) {
  const running = useAppStore((s) => !!s.running[session.id]);
  const send = useAppStore((s) => s.send);
  const stop = useAppStore((s) => s.stop);
  const [draft, setDraft] = useState('');
  const canStop = running && session.runner !== 'cloud';

  const submit = () => {
    const text = draft.trim();
    if (!text || running) return;
    setDraft('');
    void send(session.id, text);
  };

  return (
    <div
      className={cn(
        'flex items-end gap-1.5 rounded-xl border border-border bg-background px-2.5 py-1.5 transition-colors focus-within:border-muted-foreground/30',
        running && 'opacity-70',
      )}
    >
      <textarea
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            submit();
          }
        }}
        rows={1}
        disabled={running}
        placeholder={running ? 'Working…' : 'Reply…'}
        className="max-h-20 min-h-6 w-full resize-none bg-transparent py-0.5 text-[12.5px] outline-none field-sizing-content placeholder:text-muted-foreground disabled:cursor-not-allowed"
      />
      {canStop ? (
        <button
          onClick={() => void stop(session.id)}
          aria-label="Stop"
          className="mb-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground"
        >
          <Square className="size-2.5 fill-current" />
        </button>
      ) : (
        <button
          onClick={submit}
          disabled={!draft.trim() || running}
          aria-label="Send"
          className="mb-0.5 flex size-6 shrink-0 items-center justify-center rounded-full bg-primary text-primary-foreground transition-opacity disabled:bg-[#d9d9d9] disabled:opacity-100"
        >
          <ArrowUp className="size-3.5" />
        </button>
      )}
    </div>
  );
}

/* ── One thread filling the main area ─────────────────────── */

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
      <WithContext session={session}>
        <div className="relative flex flex-1 flex-col items-center justify-center px-6 pb-24">
          <h1 className="mb-7 text-[26px] font-semibold tracking-tight text-foreground">What should we work on?</h1>
          <div className="w-full max-w-[600px]">
            <Composer session={session} autoFocus />
          </div>
          {overId === session.id && <DropOverlay />}
        </div>
      </WithContext>
    );

  return (
    <WithContext session={session}>
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
    </WithContext>
  );
}

/** The thread with its Context panel on the right; the toggle sits top-right of the thread. */
function WithContext({ session, children }: { session: Session; children: React.ReactNode }) {
  const open = useContextPanel((s) => s.open);
  return (
    <div className="flex min-h-0 flex-1">
      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
        {children}
        <ContextToggle session={session} className="absolute top-2 right-3 z-10" />
      </div>
      {open && <ContextPanel session={session} />}
    </div>
  );
}
