/**
 * Split view: what is on screen, and where — threads, MCP apps and plugin
 * pages. The layout is a small
 * tree — a pane, or a row/column of subtrees with relative sizes — capped at
 * four panes. Every change is a pure tree edit followed by `normalize`, so a
 * row inside a row never survives and a split with one child collapses.
 *
 * Selecting a thread anywhere (sidebar, ⌘T, inbox, plugins) lands it in the
 * focused pane, or focuses the pane that already shows it.
 */
import { create } from 'zustand';
import { useAppStore } from '@/store';
import type { AppRef } from './pinnedApps';

export const MAX_PANES = 4;

export type Zone = 'left' | 'right' | 'top' | 'bottom' | 'center';
/** What a pane shows. Apps carry their whole ref, so a pane opens them without the Apps panel. */
export type PaneContent = { kind: 'thread'; id: string } | { kind: 'app'; app: AppRef } | { kind: 'plugin'; id: string };
export interface Leaf {
  kind: 'leaf';
  key: string;
  /** null: an empty pane waiting for a thread */
  content: PaneContent | null;
}
export interface Split {
  kind: 'split';
  dir: 'row' | 'col';
  children: Node[];
  /** fractions of the split's length, one per child, summing to 1 */
  sizes: number[];
}
export type Node = Leaf | Split;

export interface DropTarget {
  paneKey: string;
  zone: Zone;
  /** where the overlay goes, in viewport pixels */
  rect: { left: number; top: number; width: number; height: number };
}
export interface Drag {
  item: PaneContent;
  x: number;
  y: number;
  target: DropTarget | null;
}

let seq = 0;
const newKey = () => `p${++seq}`;
const leaf = (content: PaneContent | null): Leaf => ({ kind: 'leaf', key: newKey(), content });
const equal = (n: number) => Array.from({ length: n }, () => 1 / n);
export const thread = (id: string): PaneContent => ({ kind: 'thread', id });

/** One identity per thing, so the same app or thread is never open twice. */
export function contentId(c: PaneContent | null): string | null {
  if (!c) return null;
  return c.kind === 'thread' ? `thread:${c.id}` : c.kind === 'app' ? `app:${c.app.key}` : `plugin:${c.id}`;
}
export const threadOf = (l: Leaf): string | null => (l.content?.kind === 'thread' ? l.content.id : null);
/** threads open in panes, optionally leaving one pane out */
export const threadIds = (n: Node, except?: string): string[] =>
  leaves(n).flatMap((l) => (l.key !== except && threadOf(l) ? [threadOf(l)!] : []));

export function leaves(n: Node): Leaf[] {
  return n.kind === 'leaf' ? [n] : n.children.flatMap(leaves);
}

function mapLeaves(n: Node, fn: (l: Leaf) => Leaf): Node {
  return n.kind === 'leaf' ? fn(n) : { ...n, children: n.children.map((c) => mapLeaves(c, fn)) };
}

/** Collapse one-child splits and fold a split into a parent running the same way. */
function normalize(n: Node): Node {
  if (n.kind === 'leaf') return n;
  const children: Node[] = [];
  const sizes: number[] = [];
  n.children.forEach((c, i) => {
    const m = normalize(c);
    if (m.kind === 'split' && m.dir === n.dir) {
      m.children.forEach((cc, j) => {
        children.push(cc);
        sizes.push(n.sizes[i] * m.sizes[j]);
      });
    } else {
      children.push(m);
      sizes.push(n.sizes[i]);
    }
  });
  if (children.length === 1) return children[0];
  const total = sizes.reduce((a, b) => a + b, 0) || 1;
  return { ...n, children, sizes: sizes.map((s) => s / total) };
}

function removeLeaf(n: Node, key: string): Node | null {
  if (n.kind === 'leaf') return n.key === key ? null : n;
  const children: Node[] = [];
  const sizes: number[] = [];
  n.children.forEach((c, i) => {
    const m = removeLeaf(c, key);
    if (m) {
      children.push(m);
      sizes.push(n.sizes[i]);
    }
  });
  if (!children.length) return null;
  return normalize({ ...n, children, sizes });
}

/** Put `add` on one side of the pane `target`. */
function insertAt(n: Node, target: string, zone: Exclude<Zone, 'center'>, add: Leaf): Node {
  const dir = zone === 'left' || zone === 'right' ? 'row' : 'col';
  const before = zone === 'left' || zone === 'top';
  const visit = (m: Node): Node => {
    if (m.kind === 'leaf')
      return m.key === target ? { kind: 'split', dir, children: before ? [add, m] : [m, add], sizes: [0.5, 0.5] } : m;
    return { ...m, children: m.children.map(visit) };
  };
  return normalize(visit(n));
}

/** The layout's shape, e.g. `row(L,col(L,L))` — sizes are remembered per shape. */
function shape(n: Node): string {
  return n.kind === 'leaf' ? 'L' : `${n.dir}(${n.children.map(shape).join(',')})`;
}
function collectSizes(n: Node, out: number[][] = []): number[][] {
  if (n.kind === 'split') {
    out.push(n.sizes);
    n.children.forEach((c) => collectSizes(c, out));
  }
  return out;
}
function applySizes(n: Node, list: number[][] | undefined): Node {
  let i = 0;
  const visit = (m: Node): Node => {
    if (m.kind === 'leaf') return m;
    const saved = list?.[i++];
    const sizes = saved?.length === m.children.length ? saved : equal(m.children.length);
    return { ...m, sizes, children: m.children.map(visit) };
  };
  return visit(n);
}

const SIZES_KEY = 'aa.splitSizes';
function loadSizes(): Record<string, number[][]> {
  try {
    // The fixed 1–4 pane layouts are gone; their setting means nothing now.
    localStorage.removeItem('aa.panes');
    return JSON.parse(localStorage.getItem(SIZES_KEY) ?? '{}');
  } catch {
    return {};
  }
}
const savedSizes = loadSizes();
function saveSizes(tree: Node) {
  if (tree.kind === 'leaf') return;
  savedSizes[shape(tree)] = collectSizes(tree);
  try {
    localStorage.setItem(SIZES_KEY, JSON.stringify(savedSizes));
  } catch {
    /* best-effort */
  }
}
/** A new shape starts from the sizes it last had, or equal ones. */
const withSavedSizes = (tree: Node) => applySizes(tree, savedSizes[shape(tree)]);

function updateSplit(n: Node, path: number[], fn: (s: Split) => Split): Node {
  if (n.kind === 'leaf') return n;
  if (!path.length) return fn(n);
  const [i, ...rest] = path;
  return { ...n, children: n.children.map((c, j) => (j === i ? updateSplit(c, rest, fn) : c)) };
}

interface PanesState {
  tree: Node;
  focusedKey: string;
  /** a pane enlarged to fill the main area (⌘↵); Esc / ⌘↵ restores */
  maximized: string | null;
  drag: Drag | null;

  /** a pane being resized: the capture layer keeps iframes from eating the pointer */
  resizing: 'row' | 'col' | null;

  /** make `key` the focused pane and select its thread */
  focus: (key: string) => void;
  /** the selected thread changed: show it in the focused pane, or focus its pane */
  show: (sessionId: string) => void;
  close: (key: string) => void;
  /** empty a pane (the last one can't close) */
  clear: (key: string) => void;
  /** replace the whole layout, e.g. with a saved group's */
  restore: (tree: Node, focusedKey?: string) => void;
  /** keep just the focused pane */
  single: () => void;
  /** add an empty pane beside `key`, focused; false when there is no room */
  split: (key: string, zone: Exclude<Zone, 'center'>) => boolean;
  /** drop threads that just went away (deleted, other workspace) */
  prune: (gone: Set<string>) => void;
  toggleMaximize: () => void;
  setSizes: (path: number[], sizes: number[], persist?: boolean) => void;
  resetSizes: (path: number[]) => void;
  setDrag: (drag: Drag | null) => void;
  setResizing: (dir: 'row' | 'col' | null) => void;
  drop: (item: PaneContent, target: DropTarget) => void;
  /** per app pane, the pane its app links opened (a new link replaces that pane's view) */
  linked: Record<string, string>;
  /** an app in pane `source` opened another view: show it in the pane to its right */
  openLinked: (source: string, app: AppRef) => void;
}

const first = leaf(null);

export const usePanes = create<PanesState>((set, get) => {
  const relayout = (tree: Node, focusedKey: string) => {
    set({ tree: withSavedSizes(tree), maximized: null });
    get().focus(focusedKey);
  };

  return {
    tree: first,
    focusedKey: first.key,
    maximized: null,
    drag: null,
    resizing: null,
    linked: {},

    focus: (key) => {
      const l = leaves(get().tree).find((x) => x.key === key);
      if (!l) return;
      if (get().focusedKey !== key) set({ focusedKey: key });
      const id = threadOf(l);
      const app = useAppStore.getState();
      // A deleted thread (a group's dangling pane) is not selectable.
      if (id && app.selectedId !== id && app.sessions.some((s) => s.id === id)) app.select(id);
    },

    show: (sessionId) => {
      const { tree, focusedKey } = get();
      const open = leaves(tree).find((l) => threadOf(l) === sessionId);
      if (open) return open.key !== focusedKey ? set({ focusedKey: open.key }) : undefined;
      set({ tree: mapLeaves(tree, (l) => (l.key === focusedKey ? { ...l, content: thread(sessionId) } : l)) });
    },

    clear: (key) => set({ tree: mapLeaves(get().tree, (l) => (l.key === key ? { ...l, content: null } : l)) }),

    restore: (tree, focusedKey) => {
      // Fresh keys: a saved tree's could clash with panes made since.
      const keys = new Map<string, string>();
      const next = mapLeaves(tree, (l) => {
        keys.set(l.key, newKey());
        return { ...l, key: keys.get(l.key)! };
      });
      set({ tree: next, maximized: null });
      get().focus(keys.get(focusedKey ?? '') ?? leaves(next)[0].key);
    },

    single: () => {
      const { tree, focusedKey } = get();
      const keep = leaves(tree).find((l) => l.key === focusedKey) ?? leaves(tree)[0];
      relayout(keep, keep.key);
    },

    close: (key) => {
      const { tree, focusedKey } = get();
      const list = leaves(tree);
      if (list.length < 2) return;
      const next = removeLeaf(tree, key)!;
      const i = list.findIndex((l) => l.key === key);
      // Focus moves to the neighbour, like closing a tab.
      const focus = key === focusedKey ? (list[i - 1] ?? list[i + 1]).key : focusedKey;
      relayout(next, focus);
    },

    split: (key, zone) => {
      const { tree } = get();
      if (leaves(tree).length >= MAX_PANES) return false;
      const add = leaf(null);
      set({ tree: withSavedSizes(insertAt(tree, key, zone, add)), maximized: null, focusedKey: add.key });
      return true;
    },

    prune: (ids) => {
      const { tree, focusedKey } = get();
      const list = leaves(tree);
      const isGone = (l: Leaf) => !!threadOf(l) && ids.has(threadOf(l)!);
      const gone = list.filter(isGone);
      if (!gone.length) return;
      let next: Node | null = tree;
      for (const l of gone) next = next && leaves(next).length > 1 ? removeLeaf(next, l.key) : next;
      // The last pane stays, empty, and gets a fresh thread.
      next = mapLeaves(next!, (l) => (isGone(l) ? { ...l, content: null } : l));
      const kept = leaves(next);
      const focus = kept.some((l) => l.key === focusedKey) ? focusedKey : kept[0].key;
      relayout(next, focus);
    },

    toggleMaximize: () => {
      const { tree, maximized, focusedKey } = get();
      if (tree.kind === 'leaf') return set({ maximized: null });
      set({ maximized: maximized ? null : focusedKey });
    },

    setSizes: (path, sizes, persist) => {
      const tree = updateSplit(get().tree, path, (s) => ({ ...s, sizes }));
      set({ tree });
      if (persist) saveSizes(tree);
    },
    resetSizes: (path) => {
      const tree = updateSplit(get().tree, path, (s) => ({ ...s, sizes: equal(s.children.length) }));
      set({ tree });
      saveSizes(tree);
    },

    setDrag: (drag) => set({ drag }),
    setResizing: (resizing) => set({ resizing }),

    drop: (item, { paneKey, zone }) => {
      const { tree } = get();
      const list = leaves(tree);
      const target = list.find((l) => l.key === paneKey);
      if (!target) return;
      // Something already on screen moves with its pane instead of opening twice.
      const from = list.find((l) => contentId(l.content) === contentId(item));
      if (zone === 'center') {
        const next = mapLeaves(tree, (l) =>
          l.key === target.key
            ? { ...l, content: item }
            : from && l.key === from.key
              ? { ...l, content: target.content }
              : l,
        );
        set({ tree: next, maximized: null });
        return get().focus(target.key);
      }
      if (from?.key === target.key) return get().focus(target.key);
      if (!from && list.length >= MAX_PANES) return;
      const add: Leaf = from ? { ...from } : leaf(item);
      const rest = from ? removeLeaf(tree, from.key)! : tree;
      relayout(insertAt(rest, target.key, zone, add), add.key);
    },

    openLinked: (source, app) => {
      const { tree, linked } = get();
      const list = leaves(tree);
      if (!list.some((l) => l.key === source)) return;
      const item: PaneContent = { kind: 'app', app };
      // Already on screen: never open the same view twice.
      const open = list.find((l) => contentId(l.content) === contentId(item));
      if (open) return get().focus(open.key);
      // The pane this source opened before (unless something else was put
      // there since); at the pane cap, the rightmost other one.
      const reuse =
        list.find((l) => l.key === linked[source] && l.key !== source && l.content?.kind === 'app') ??
        (list.length >= MAX_PANES ? list.filter((l) => l.key !== source).pop() : undefined);
      if (reuse) {
        set({
          tree: mapLeaves(tree, (l) => (l.key === reuse.key ? { ...l, content: item } : l)),
          linked: { ...linked, [source]: reuse.key },
          maximized: null,
        });
        return get().focus(reuse.key);
      }
      const add = leaf(item);
      set({ linked: { ...linked, [source]: add.key } });
      relayout(insertAt(tree, source, 'right', add), add.key);
    },
  };
});

/** Which drop zone of `rect` the point is in; edges only where a split is allowed. */
export function zoneAt(
  rect: DOMRect,
  x: number,
  y: number,
  opts: { edges: boolean; vertical: boolean },
): Zone {
  if (!opts.edges) return 'center';
  const rx = (x - rect.left) / rect.width;
  const ry = (y - rect.top) / rect.height;
  const d: [Zone, number][] = [
    ['left', rx],
    ['right', 1 - rx],
  ];
  if (opts.vertical) d.push(['top', ry], ['bottom', 1 - ry]);
  const [zone, dist] = d.reduce((a, b) => (b[1] < a[1] ? b : a));
  return dist < 0.3 ? zone : 'center';
}
