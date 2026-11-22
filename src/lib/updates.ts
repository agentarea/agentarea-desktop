import { create } from 'zustand';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { getVersion } from '@tauri-apps/api/app';
import { listen } from '@tauri-apps/api/event';

type Status = 'idle' | 'checking' | 'available' | 'latest' | 'failed';

interface UpdatesState {
  status: Status;
  /** Asked for from the menu: the result is shown even when there is nothing new. */
  manual: boolean;
  update: Update | null;
  version: string | null;
  error: string | null;
  check: (manual: boolean) => Promise<void>;
  dismiss: () => void;
}

export const useUpdates = create<UpdatesState>((set, get) => ({
  status: 'idle',
  manual: false,
  update: null,
  version: null,
  error: null,
  check: async (manual) => {
    if (get().status === 'checking') return;
    set({ status: 'checking', manual, error: null });
    try {
      const update = await check();
      if (update) set({ status: 'available', update });
      else set({ status: manual ? 'latest' : 'idle', update: null });
    } catch (e) {
      console.error('update check failed', e);
      set({ status: manual ? 'failed' : 'idle', error: e instanceof Error ? e.message : String(e) });
    }
  },
  dismiss: () => set({ status: 'idle', manual: false }),
}));

void getVersion()
  .then((version) => useUpdates.setState({ version }))
  .catch(() => undefined); // not running inside Tauri

// "Check for Updates…" in the app menu (src-tauri/src/lib.rs app_menu).
const unlisten = listen('check-for-updates', () => void useUpdates.getState().check(true)).catch(() => undefined);
import.meta.hot?.dispose(() => void unlisten.then((off) => off?.()));
