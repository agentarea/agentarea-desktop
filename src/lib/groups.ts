/**
 * Groups: a split saved under a name, to come back to — its panes (threads,
 * apps, plugin pages) and divider sizes. While a group is open, every layout
 * change saves to it. Groups live per environment + workspace, next to that
 * workspace's threads (`groupsKey`), and outlive the threads in them: a
 * deleted thread leaves a placeholder pane, not a broken group.
 */
import { create } from 'zustand';
import { groupsKey, useAppStore } from '@/store';
import { leaves, usePanes, type Node, type PaneContent } from './panes';
import { usePlugins } from './plugins';

export interface PaneGroup {
  id: string;
  name: string;
  tree: Node;
  focusedKey: string;
}

function load(): PaneGroup[] {
  try {
    const raw = JSON.parse(localStorage.getItem(groupsKey()) ?? '[]');
    return Array.isArray(raw) ? raw : [];
  } catch {
    return [];
  }
}

// Resizing changes the layout on every pointer move; write once it settles.
let pending: { key: string; groups: PaneGroup[] } | null = null;
let timer: ReturnType<typeof setTimeout> | undefined;
function flush() {
  clearTimeout(timer);
  if (!pending) return;
  try {
    localStorage.setItem(pending.key, JSON.stringify(pending.groups));
  } catch {
    /* best-effort */
  }
  pending = null;
}
function saveSoon(groups: PaneGroup[]) {
  // The key is taken now: a workspace switch before the write must not move it.
  pending = { key: groupsKey(), groups };
  clearTimeout(timer);
  timer = setTimeout(flush, 250);
}

/** What a pane is called, for a group's default name. */
export function contentTitle(c: PaneContent): string {
  if (c.kind === 'app') return c.app.title;
  if (c.kind === 'plugin') return usePlugins.getState().plugins.find((p) => p.id === c.id)?.title ?? c.id;
  return useAppStore.getState().sessions.find((s) => s.id === c.id)?.title ?? 'Deleted thread';
}

/** "Auth refactor + Outreach" — from the panes' titles. */
export function defaultGroupName(tree: Node): string {
  const cut = (t: string) => (t.length > 28 ? `${t.slice(0, 27).trimEnd()}…` : t);
  const titles = leaves(tree).flatMap((l) => (l.content ? [cut(contentTitle(l.content))] : []));
  return titles.join(' + ') || 'Untitled group';
}

interface GroupsState {
  groups: PaneGroup[];
  /** the group on screen; layout changes save to it */
  activeId: string | null;
  /** the "Save group" name field is open (button or ⌘S) */
  naming: boolean;
  setNaming: (on: boolean) => void;
  /** save the current split as a new group, and keep it open */
  save: (name: string) => void;
  /** restore a group's layout exactly */
  open: (id: string) => void;
  /** leave the group, keeping just the focused pane */
  close: () => void;
  rename: (id: string, name: string) => void;
  remove: (id: string) => void;
}

export const useGroups = create<GroupsState>((set, get) => {
  const put = (groups: PaneGroup[]) => {
    set({ groups });
    saveSoon(groups);
  };
  return {
    groups: load(),
    activeId: null,
    naming: false,
    setNaming: (naming) => set({ naming }),

    save: (name) => {
      const { tree, focusedKey } = usePanes.getState();
      const group = { id: crypto.randomUUID(), name: name.trim() || defaultGroupName(tree), tree, focusedKey };
      put([group, ...get().groups]);
      set({ activeId: group.id, naming: false });
    },

    open: (id) => {
      const group = get().groups.find((g) => g.id === id);
      if (!group) return;
      set({ activeId: null, naming: false });
      usePanes.getState().restore(group.tree, group.focusedKey);
      set({ activeId: id });
    },

    close: () => {
      set({ activeId: null });
      usePanes.getState().single();
    },

    rename: (id, name) => {
      if (!name.trim()) return;
      put(get().groups.map((g) => (g.id === id ? { ...g, name: name.trim() } : g)));
    },

    remove: (id) => {
      put(get().groups.filter((g) => g.id !== id));
      if (get().activeId === id) set({ activeId: null });
    },
  };
});

// While a group is open, it follows the layout.
usePanes.subscribe((s, prev) => {
  if (s.tree === prev.tree && s.focusedKey === prev.focusedKey) return;
  const { activeId, groups } = useGroups.getState();
  if (!activeId) return;
  const next = groups.map((g) => (g.id === activeId ? { ...g, tree: s.tree, focusedKey: s.focusedKey } : g));
  useGroups.setState({ groups: next });
  saveSoon(next);
});

// Another workspace has its own groups. Runs before the old threads leave
// their panes, so an open group is let go of before it could save that.
useAppStore.subscribe((s, prev) => {
  if (s.workspaceId === prev.workspaceId) return;
  flush();
  const wasOpen = !!useGroups.getState().activeId;
  useGroups.setState({ groups: load(), activeId: null, naming: false });
  if (wasOpen) usePanes.getState().single();
});

if (typeof window !== 'undefined') window.addEventListener('beforeunload', flush);
