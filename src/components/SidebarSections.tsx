/**
 * The sidebar's reorderable middle: section wrappers, their headers and menus,
 * plugin entries with an inline, resizable panel, and the drag that reorders
 * them. Dragging a plugin entry out of the sidebar hands it to the split-view
 * drag instead, so it can still be dropped into a pane.
 */
import { createContext, useContext, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { ArrowUp, ArrowUpToLine, ArrowDown, ChevronRight, EyeOff, Ellipsis, PanelTopClose, PanelTopOpen } from 'lucide-react';
import { cn } from '@/lib/utils';
import { usePanes, type PaneContent } from '@/lib/panes';
import type { Plugin } from '@/lib/plugins';
import { usePlugins } from '@/lib/plugins';
import {
  isCollapsed,
  isPluginSection,
  PANEL_DEFAULT,
  PANEL_MIN,
  pluginSection,
  useSidebarLayout,
} from '@/lib/sidebarLayout';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu';
import { DRAG_THRESHOLD, swallowClick, targetAt } from './SplitView';
import { PluginIcon } from './PluginIcon';
import { PluginFrame } from './PluginFrame';

/** the section ids on screen, top to bottom (for Move up / down) */
export const SectionOrder = createContext<string[]>([]);

/** pixels from the scroll area's top or bottom edge where a drag scrolls it */
const EDGE = 32;

/**
 * Start tracking a press on a section header or plugin entry. Past
 * DRAG_THRESHOLD it becomes a drag: over the sidebar it shows where the
 * section would go; a plugin entry (`item`) taken over the main area becomes
 * a split-view drag. Esc cancels.
 */
export function startSectionDrag(e: ReactPointerEvent, id: string, title: string, item?: PaneContent) {
  const hit = (e.target as HTMLElement).closest('button, textarea, input, a, [role=menuitem]');
  if (e.button !== 0 || (hit && hit !== e.currentTarget)) return;
  const aside = (e.currentTarget as HTMLElement).closest<HTMLElement>('[data-sidebar]');
  const scroller = aside?.querySelector<HTMLElement>('[data-slot=scroll-area-viewport]');
  if (!aside || !scroller) return;
  const x0 = e.clientX;
  const y0 = e.clientY;
  let x = x0;
  let y = y0;
  let dragging = false;
  let visible: string[] = [];
  let insertAt: number | null = null;
  let raf = 0;
  const layout = useSidebarLayout.getState;
  const panes = usePanes.getState;

  const update = () => {
    const a = aside.getBoundingClientRect();
    const inSidebar = x >= a.left && x <= a.right && y >= a.top && y <= a.bottom;
    if (item && !inSidebar) {
      insertAt = null;
      layout().setDrag({ id, title, x, y, line: null });
      panes().setDrag({ item, x, y, target: targetAt(item, x, y) });
      return;
    }
    const els = [...scroller.querySelectorAll<HTMLElement>('[data-section-id]')];
    visible = els.map((el) => el.dataset.sectionId!);
    const rects = els.map((el) => el.getBoundingClientRect());
    let i = rects.findIndex((r) => y < r.top + r.height / 2);
    if (i < 0) i = rects.length;
    const from = visible.indexOf(id);
    insertAt = i === from || i === from + 1 ? null : i;
    let line = null;
    if (insertAt !== null && rects.length) {
      const s = scroller.getBoundingClientRect();
      // Middle of the gap above the section it would land before (or below the last).
      const at =
        insertAt < rects.length
          ? rects[insertAt].top + parseFloat(getComputedStyle(els[insertAt]).paddingTop) / 2
          : rects[rects.length - 1].bottom + 3;
      line = { top: Math.min(Math.max(at, s.top + 4), s.bottom - 4), left: s.left + 14, width: s.width - 28 };
    }
    layout().setDrag({ id, title, x, y, line });
    if (item && panes().drag) panes().setDrag({ item, x, y, target: null });
  };

  // Near the scroll area's edges, keep scrolling while the pointer rests there.
  const tick = () => {
    const s = scroller.getBoundingClientRect();
    const a = aside.getBoundingClientRect();
    const inSidebar = !item || (x >= a.left && x <= a.right);
    const d = y < s.top + EDGE ? y - (s.top + EDGE) : y > s.bottom - EDGE ? y - (s.bottom - EDGE) : 0;
    if (inSidebar && d) {
      const before = scroller.scrollTop;
      scroller.scrollTop += Math.max(-16, Math.min(16, d / 2));
      if (scroller.scrollTop !== before) update();
    }
    raf = requestAnimationFrame(tick);
  };

  const move = (ev: PointerEvent) => {
    x = ev.clientX;
    y = ev.clientY;
    if (!dragging) {
      if (Math.hypot(x - x0, y - y0) < DRAG_THRESHOLD) return;
      dragging = true;
      document.body.classList.add('pane-dragging');
      raf = requestAnimationFrame(tick);
    }
    update();
  };
  const end = (drop: boolean) => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    window.removeEventListener('pointercancel', cancel);
    window.removeEventListener('keydown', key, true);
    cancelAnimationFrame(raf);
    if (!dragging) return;
    document.body.classList.remove('pane-dragging');
    const target = panes().drag?.target;
    layout().setDrag(null);
    if (panes().drag) panes().setDrag(null);
    if (drop && item && target) panes().drop(item, target);
    else if (drop && insertAt !== null) layout().move(visible, id, insertAt);
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

/**
 * The insertion line and a ghost while a section is dragged within the
 * sidebar, over a capture layer so plugin panels (iframes) don't swallow the
 * pointer. Out over the main area, the split view's DragLayer takes over.
 */
export function SidebarDragLayer() {
  const drag = useSidebarLayout((s) => s.drag);
  const paneDrag = usePanes((s) => !!s.drag);
  const plugin = usePlugins((s) => (drag && isPluginSection(drag.id) ? s.plugins.find((p) => pluginSection(p.id) === drag.id) : undefined));
  if (!drag) return null;
  return (
    <>
      {!paneDrag && <div className="fixed inset-0 z-30" />}
      {drag.line && (
        <div
          className="pointer-events-none fixed z-50 h-[2px] -translate-y-1/2 rounded-full bg-drop shadow-[0_0_0_2px_var(--sidebar)]"
          style={{ top: drag.line.top, left: drag.line.left, width: drag.line.width }}
        />
      )}
      {!paneDrag && (
        <div
          className="pointer-events-none fixed z-50 flex max-w-[220px] items-center gap-1.5 rounded-lg border border-border bg-popover/95 px-2.5 py-1.5 text-[12px] font-medium text-foreground shadow-[0_6px_20px_rgba(0,0,0,0.12)] backdrop-blur-sm"
          style={{ left: drag.x + 14, top: drag.y + 12 }}
        >
          {plugin && <PluginIcon plugin={plugin} className="size-3.5 text-muted-foreground" />}
          <span className="truncate">{drag.title}</span>
        </div>
      )}
    </>
  );
}

/** One reorderable block of the sidebar; `gap` separates it from the block above. */
export function SidebarSection({ id, gap, children }: { id: string; gap: boolean; children: ReactNode }) {
  const dragged = useSidebarLayout((s) => s.drag?.id === id);
  return (
    <div data-section-id={id} className={cn('transition-opacity', gap ? 'pt-3' : 'pt-px', dragged && 'opacity-40')}>
      {children}
    </div>
  );
}

/**
 * Non-drag fallbacks for one section: move it, collapse it, or (plugins)
 * take it out of the sidebar. Opens from its ⋯ button or a right-click on the row.
 */
function SectionMenu({
  id,
  open,
  onOpenChange,
  collapsible,
  className,
}: {
  id: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  collapsible: boolean;
  className?: string;
}) {
  const visible = useContext(SectionOrder);
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, id));
  const layout = useSidebarLayout.getState();
  const i = visible.indexOf(id);
  const plugin = isPluginSection(id);
  return (
    <DropdownMenu open={open} onOpenChange={onOpenChange}>
      <DropdownMenuTrigger asChild>
        <button
          onClick={(e) => e.stopPropagation()}
          aria-label="Section options"
          className={cn(
            'shrink-0 rounded text-muted-foreground hover:text-foreground data-[state=open]:visible data-[state=open]:text-foreground',
            className,
          )}
        >
          <Ellipsis className="size-3.5" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent
        align="end"
        className="min-w-[10.5rem]"
        // Portalled, but React still bubbles these to the row underneath.
        onClick={(e) => e.stopPropagation()}
        onPointerDown={(e) => e.stopPropagation()}
        onContextMenu={(e) => e.stopPropagation()}
      >
        <DropdownMenuItem disabled={i <= 0} onSelect={() => layout.move(visible, id, i - 1)}>
          <ArrowUp /> Move up
        </DropdownMenuItem>
        <DropdownMenuItem disabled={i < 0 || i >= visible.length - 1} onSelect={() => layout.move(visible, id, i + 2)}>
          <ArrowDown /> Move down
        </DropdownMenuItem>
        <DropdownMenuItem disabled={i <= 0} onSelect={() => layout.move(visible, id, 0)}>
          <ArrowUpToLine /> Move to top
        </DropdownMenuItem>
        {(collapsible || plugin) && <DropdownMenuSeparator />}
        {collapsible && (
          <DropdownMenuItem onSelect={() => layout.toggle(id)}>
            {collapsed ? <PanelTopOpen /> : <PanelTopClose />} {collapsed ? 'Expand' : 'Collapse'}
          </DropdownMenuItem>
        )}
        {plugin && (
          <DropdownMenuItem onSelect={() => layout.setHidden(id, true)}>
            <EyeOff /> Hide from sidebar
          </DropdownMenuItem>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

/** A section's title row: click to collapse, drag to reorder, ⋯ or right-click for the menu. */
export function SectionHeader({ id, label }: { id: string; label: string }) {
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, id));
  const [menu, setMenu] = useState(false);
  return (
    <div
      onClick={() => useSidebarLayout.getState().toggle(id)}
      onPointerDown={(e) => startSectionDrag(e, id, label)}
      onContextMenu={(e) => {
        e.preventDefault();
        setMenu(true);
      }}
      aria-expanded={!collapsed}
      className="group/h flex h-6 cursor-default items-center gap-1 rounded-md px-2.5 text-[11px] font-medium text-muted-foreground-subtle select-none hover:text-foreground"
    >
      <span className="truncate">{label}</span>
      <ChevronRight
        className={cn(
          'size-3 shrink-0 transition-transform',
          collapsed ? '' : 'rotate-90 opacity-0 group-hover/h:opacity-100',
        )}
      />
      <SectionMenu
        id={id}
        open={menu}
        onOpenChange={setMenu}
        collapsible
        className="invisible ml-auto group-hover/h:visible"
      />
    </div>
  );
}

/**
 * A plugin in the sidebar, like a pinned app: click opens its page, drag
 * reorders it (or, over the main area, opens it in a split), the chevron shows
 * its sidebar page inline with a handle to resize it.
 */
export function PluginEntry({ plugin, active }: { plugin: Plugin; active: boolean }) {
  const id = pluginSection(plugin.id);
  const collapsed = useSidebarLayout((s) => isCollapsed(s.collapsed, id));
  const height = useSidebarLayout((s) => s.heights[id] ?? PANEL_DEFAULT);
  const [menu, setMenu] = useState(false);
  const panel = plugin.sidebar;
  return (
    <>
      <div
        onClick={() => usePlugins.getState().open(plugin.id, '')}
        onPointerDown={(e) => startSectionDrag(e, id, plugin.title, { kind: 'plugin', id: plugin.id })}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu(true);
        }}
        title={plugin.description ?? plugin.title}
        className={cn(
          'group flex cursor-default items-center gap-2 rounded-lg px-2.5 py-[6px] text-[13px] transition-colors select-none',
          active ? 'bg-sidebar-accent' : 'hover:bg-sidebar-hover',
        )}
      >
        <PluginIcon plugin={plugin} className="size-[15px] text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate">{plugin.title}</span>
        <SectionMenu
          id={id}
          open={menu}
          onOpenChange={setMenu}
          collapsible={!!panel}
          className="invisible group-hover:visible"
        />
        {panel && (
          <button
            onClick={(e) => {
              e.stopPropagation();
              useSidebarLayout.getState().toggle(id);
            }}
            aria-label={collapsed ? `Show ${plugin.title} panel` : `Hide ${plugin.title} panel`}
            aria-expanded={!collapsed}
            className="-mr-1 shrink-0 rounded p-0.5 text-muted-foreground hover:bg-sidebar-accent hover:text-foreground"
          >
            <ChevronRight className={cn('size-3.5 transition-transform', !collapsed && 'rotate-90')} />
          </button>
        )}
      </div>
      {panel && !collapsed && (
        <div className="mt-0.5 mb-1 overflow-hidden rounded-lg border border-sidebar-border bg-sidebar">
          <div style={{ height }}>
            <PluginFrame plugin={plugin} page={panel} className="size-full border-0 bg-sidebar" />
          </div>
          <PanelResizeHandle id={id} height={height} />
        </div>
      )}
    </>
  );
}

/** Drag to set a plugin panel's height; double-click resets it. */
function PanelResizeHandle({ id, height }: { id: string; height: number }) {
  const start = (e: ReactPointerEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const y0 = e.clientY;
    let h = height;
    const max = () => Math.max(PANEL_MIN, window.innerHeight - 160);
    // The capture layer keeps the panel's iframe from eating the pointer.
    usePanes.getState().setResizing('col');
    document.body.classList.add('pane-resizing-col');
    const move = (ev: PointerEvent) => {
      h = Math.min(max(), Math.max(PANEL_MIN, height + ev.clientY - y0));
      useSidebarLayout.getState().setHeight(id, h, false);
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      window.removeEventListener('pointercancel', up);
      usePanes.getState().setResizing(null);
      document.body.classList.remove('pane-resizing-col');
      useSidebarLayout.getState().setHeight(id, h);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    window.addEventListener('pointercancel', up);
  };
  return (
    <div
      role="separator"
      aria-orientation="horizontal"
      aria-label="Resize panel"
      title="Drag to resize · double-click to reset"
      onPointerDown={start}
      onDoubleClick={() => useSidebarLayout.getState().setHeight(id, PANEL_DEFAULT)}
      className="group/r flex h-2.5 cursor-row-resize items-center justify-center border-t border-sidebar-border/60 hover:bg-sidebar-hover"
    >
      <span className="h-[3px] w-8 rounded-full bg-muted-foreground/25 transition-colors group-hover/r:bg-muted-foreground/60" />
    </div>
  );
}
