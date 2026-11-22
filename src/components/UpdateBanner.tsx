import { useEffect, useState } from 'react';
import { check, type Update } from '@tauri-apps/plugin-updater';
import { relaunch } from '@tauri-apps/plugin-process';
import { Button } from '@/components/ui/button';

// Checks the release feed once per launch; the update is installed only when
// the user asks, then the app restarts into it.
export function UpdateBanner() {
  const [update, setUpdate] = useState<Update | null>(null);
  const [state, setState] = useState<'idle' | 'installing' | 'failed'>('idle');

  useEffect(() => {
    if (import.meta.env.DEV) return;
    check()
      .then(setUpdate)
      .catch((err) => console.error('update check failed', err));
  }, []);

  if (!update) return null;

  const install = async () => {
    setState('installing');
    try {
      await update.downloadAndInstall();
      await relaunch();
    } catch (err) {
      console.error('update install failed', err);
      setState('failed');
    }
  };

  return (
    <div className="fixed right-4 bottom-4 z-50 flex items-center gap-3 rounded-lg border bg-popover px-4 py-3 text-sm text-popover-foreground shadow-lg">
      <span>
        {state === 'failed' ? 'Update failed.' : `AgentArea Desktop ${update.version} is available.`}
      </span>
      <Button size="sm" onClick={install} disabled={state === 'installing'}>
        {state === 'installing' ? 'Updating…' : state === 'failed' ? 'Retry' : 'Update and restart'}
      </Button>
      {state !== 'installing' && (
        <Button size="sm" variant="ghost" onClick={() => setUpdate(null)}>
          Later
        </Button>
      )}
    </div>
  );
}
