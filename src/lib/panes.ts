/**
 * Split view: which threads are on screen, and where. The layout is a small
 * tree — a pane, or a row/column of subtrees with relative sizes — capped at
 * four panes. Every change is a pure tree edit followed by `normalize`, so a
 * row inside a row never survives and a split with one child collapses.
 *
 * The focused pane always shows the app's selected thread: selecting a thread
 * anywhere (sidebar, ⌘T, inbox, plugins) lands it in the focused pane, or
 * focuses the pane that already shows it.
 */
import { create } from 'zustand';
import { useAppStore } from '@/store';

export const MAX_PANES = 4;

export type Zone = 'left' | 'right' | 'top' | 'bottom' | 'center';
export interface Leaf {
  kind: 'leaf';
  key: string;
  sessionId: string | null;
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
  sessionId: string;
  x: number;
  y: number;
  target: DropTarget | null;
}

let seq = 0;
const newKey = () => `p${++seq}`;
const leaf = (sessionId: string | null): Leaf => ({ kind: 'leaf', key: newKey(), sessionId });
const equal = (n: number) => Array.from({ length: n }, () => 1 / n);

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

  /** make `key` the focused pane and select its thread */
  focus: (key: string) => void;
  /** the selected thread changed: show it in the focused pane, or focus its pane */
  show: (sessionId: string) => void;
  close: (key: string) => void;
  /** add an empty pane beside `key`, focused; false when there is no room */
  split: (key: string, zone: Exclude<Zone, 'center'>) => boolean;
  /** drop threads that no longer exist (deleted, other workspace) */
  prune: (ids: Set<string>) => void;
  toggleMaximize: () => void;
  setSizes: (path: number[], sizes: number[], persist?: boolean) => void;
  resetSizes: (path: number[]) => void;
  setDrag: (drag: Drag | null) => void;
  drop: (sessionId: string, target: DropTarget) => void;
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

    focus: (key) => {
      const l = leaves(get().tree).find((x) => x.key === key);
      if (!l) return;
      if (get().focusedKey !== key) set({ focusedKey: key });
      if (l.sessionId && useAppStore.getState().selectedId !== l.sessionId) useAppStore.getState().select(l.sessionId);
    },

    show: (sessionId) => {
      const { tree, focusedKey } = get();
      const open = leaves(tree).find((l) => l.sessionId === sessionId);
      if (open) return open.key !== focusedKey ? set({ focusedKey: open.key }) : undefined;
      set({ tree: mapLeaves(tree, (l) => (l.key === focusedKey ? { ...l, sessionId } : l)) });
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
      const gone = list.filter((l) => l.sessionId && !ids.has(l.sessionId));
      if (!gone.length) return;
      let next: Node | null = tree;
      for (const l of gone) next = next && leaves(next).length > 1 ? removeLeaf(next, l.key) : next;
      // The last pane stays, empty, and gets a fresh thread.
      next = mapLeaves(next!, (l) => (l.sessionId && !ids.has(l.sessionId) ? { ...l, sessionId: null } : l));
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

    drop: (sessionId, { paneKey, zone }) => {
      const { tree } = get();
      const list = leaves(tree);
      const target = list.find((l) => l.key === paneKey);
      if (!target) return;
      // A thread already on screen moves with its pane instead of opening twice.
      const from = list.find((l) => l.sessionId === sessionId);
      if (zone === 'center') {
        const next = mapLeaves(tree, (l) =>
          l.key === target.key ? { ...l, sessionId } : from && l.key === from.key ? { ...l, sessionId: target.sessionId } : l,
        );
        set({ tree: next, maximized: null });
        return get().focus(target.key);
      }
      if (from?.key === target.key) return get().focus(target.key);
      if (!from && list.length >= MAX_PANES) return;
      const add: Leaf = from ? { ...from } : leaf(sessionId);
      const rest = from ? removeLeaf(tree, from.key)! : tree;
      relayout(insertAt(rest, target.key, zone, add), add.key);
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
