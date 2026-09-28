/**
 * The sidebar's middle area: which sections it shows (Pinned apps, one entry
 * per plugin, Groups, Threads), in what order, which are collapsed, and how
 * tall each plugin's inline panel is. Saved in localStorage, best-effort.
 *
 * Section ids: 'pinned' | 'groups' | 'threads' | 'plugin:<id>'.
 */
import { create } from 'zustand';

const ORDER_KEY = 'aa.sidebarOrder';
const COLLAPSED_KEY = 'aa.sidebarCollapsed';
const HIDDEN_KEY = 'aa.sidebarHidden';
const HEIGHTS_KEY = 'aa.sidebarPanelHeights';
/** the old bottom plugin sections' collapsed state, migrated once */
const LEGACY_PLUGIN_SECTIONS_KEY = 'aa.pluginSections';

/** a plugin's inline sidebar panel, in pixels */
export const PANEL_DEFAULT = 280;
export const PANEL_MIN = 120;

export const pluginSection = (id: string) => `plugin:${id}`;
export const isPluginSection = (id: string) => id.startsWith('plugin:');

function read<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

function write(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    /* best-effort */
  }
}

function initialCollapsed(): Record<string, boolean> {
  const saved = read<Record<string, boolean> | null>(COLLAPSED_KEY, null);
  if (saved) return saved;
  const legacy = read<Record<string, boolean>>(LEGACY_PLUGIN_SECTIONS_KEY, {});
  return Object.fromEntries(Object.entries(legacy).map(([id, c]) => [pluginSection(id), c]));
}

/**
 * `base` in its order, plus each item of `reference` it lacks, placed after
 * the nearest item that precedes it in `reference` (or first). Keeps a saved
 * order stable as sections come and go.
 */
export function fillOrder(base: string[], reference: string[]): string[] {
  const out = [...base];
  reference.forEach((id, i) => {
    if (out.includes(id)) return;
    let at = 0;
    for (let j = i - 1; j >= 0; j--) {
      const k = out.indexOf(reference[j]);
      if (k >= 0) {
        at = k + 1;
        break;
      }
    }
    out.splice(at, 0, id);
  });
  return out;
}

/** Plugin panels start closed; the other sections start open. */
export const isCollapsed = (collapsed: Record<string, boolean>, id: string) => collapsed[id] ?? isPluginSection(id);

export interface SidebarDrag {
  id: string;
  title: string;
  x: number;
  y: number;
  /** the insertion line, in viewport pixels; null when the drop would change nothing */
  line: { top: number; left: number; width: number } | null;
}

interface SidebarLayout {
  /** every section id ever placed, including ones not shown right now */
  order: string[];
  collapsed: Record<string, boolean>;
  /** plugin sections taken out of the sidebar (shown again from the Plugins panel) */
  hidden: string[];
  heights: Record<string, number>;
  drag: SidebarDrag | null;

  /** move `id` so it lands before `visible[to]` (or last); `visible` is the order on screen */
  move: (visible: string[], id: string, to: number) => void;
  toggle: (id: string) => void;
  setHidden: (id: string, hidden: boolean) => void;
  setHeight: (id: string, height: number, persist?: boolean) => void;
  setDrag: (drag: SidebarDrag | null) => void;
}

export const useSidebarLayout = create<SidebarLayout>((set, get) => ({
  order: read<string[]>(ORDER_KEY, []),
  collapsed: initialCollapsed(),
  hidden: read<string[]>(HIDDEN_KEY, []),
  heights: read<Record<string, number>>(HEIGHTS_KEY, {}),
  drag: null,

  move: (visible, id, to) => {
    const from = visible.indexOf(id);
    if (from < 0 || to === from || to === from + 1) return;
    const next = visible.filter((x) => x !== id);
    next.splice(to > from ? to - 1 : to, 0, id);
    const order = fillOrder(next, fillOrder(get().order, visible));
    write(ORDER_KEY, order);
    set({ order });
  },
  toggle: (id) => {
    const collapsed = { ...get().collapsed, [id]: !isCollapsed(get().collapsed, id) };
    write(COLLAPSED_KEY, collapsed);
    set({ collapsed });
  },
  setHidden: (id, hidden) => {
    const list = get().hidden.filter((x) => x !== id);
    if (hidden) list.push(id);
    write(HIDDEN_KEY, list);
    set({ hidden: list });
  },
  setHeight: (id, height, persist = true) => {
    const heights = { ...get().heights, [id]: Math.round(height) };
    if (persist) write(HEIGHTS_KEY, heights);
    set({ heights });
  },
  setDrag: (drag) => set({ drag }),
}));
