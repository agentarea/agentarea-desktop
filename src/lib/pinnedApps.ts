/**
 * MCP Apps pinned to the sidebar for one-click use. Each pin carries what is
 * needed to open the app without the Apps screen: where it runs (a local server
 * or an AgentArea instance), its entry tool and its `ui://` resource.
 */
import { create } from 'zustand';

export interface AppRef {
  /** `local:<server>/<tool>` or `cloud:<instance>/<tool>` */
  key: string;
  kind: 'local' | 'cloud';
  title: string;
  /** server / instance name */
  subtitle: string;
  icons?: string[];
  seed: string;
  toolName: string;
  resourceUri: string;
  requiresInput: boolean;
  /** local apps */
  serverId?: string;
  /** cloud apps */
  instanceId?: string;
  /** entry tool arguments: a view another app opened through an app link */
  args?: Record<string, unknown>;
}

const KEY = 'aa.pinnedApps';

function load(): AppRef[] {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? '[]');
  } catch {
    return [];
  }
}

interface PinnedApps {
  pinned: AppRef[];
  toggle: (app: AppRef) => void;
  unpin: (key: string) => void;
  /** the app to show full-width (App switches to it); `app` when opened from the Apps screen, pinned or not */
  opened: { key: string; at: number; app?: AppRef } | null;
  open: (key: string, app?: AppRef) => void;
}

export const usePinnedApps = create<PinnedApps>((set, get) => {
  const save = (pinned: AppRef[]) => {
    set({ pinned });
    try {
      localStorage.setItem(KEY, JSON.stringify(pinned));
    } catch {
      /* best-effort */
    }
  };
  return {
    pinned: load(),
    toggle: (app) => {
      const { pinned } = get();
      save(pinned.some((p) => p.key === app.key) ? pinned.filter((p) => p.key !== app.key) : [...pinned, app]);
    },
    unpin: (key) => save(get().pinned.filter((p) => p.key !== key)),
    opened: null,
    open: (key, app) => set({ opened: { key, at: Date.now(), app } }),
  };
});
