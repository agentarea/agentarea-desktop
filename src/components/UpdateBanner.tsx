import { useEffect, useState, type ReactNode } from 'react';
import { relaunch } from '@tauri-apps/plugin-process';
import { Button } from '@/components/ui/button';
import { useUpdates } from '@/lib/updates';

// Checks the release feed once per launch and whenever the account menu asks;
// an update is installed only when the user says so, then the app restarts into it.
export function UpdateBanner() {
  const { status, manual, update, version, error, check, dismiss } = useUpdates();
  const [installing, setInstalling] = useState(false);
  const [installError, setInstallError] = useState<string | null>(null);

  useEffect(() => {
    if (!import.meta.env.DEV) void check(false);
  }, [check]);

  useEffect(() => {
    if (status !== 'latest') return;
    const t = setTimeout(dismiss, 4000);
    return () => clearTimeout(t);
  }, [status, dismiss]);

  const install = async () => {
    if (!update) return;
    setInstalling(true);
    setInstallError(null);
    try {
      await update.downloadAndInstall();
      await relaunch();
    } catch (e) {
      console.error('update install failed', e);
      setInstallError(e instanceof Error ? e.message : String(e));
      setInstalling(false);
    }
  };

  let text: string;
  let actions: ReactNode = null;
  if (status === 'available' && update) {
    text = installError ? 'Update failed.' : `AgentArea Desktop ${update.version} is available.`;
    actions = (
      <>
        <Button size="sm" onClick={install} disabled={installing}>
          {installing ? 'Updating…' : installError ? 'Retry' : 'Update and restart'}
        </Button>
        {!installing && (
          <Button size="sm" variant="ghost" onClick={dismiss}>
            Later
          </Button>
        )}
      </>
    );
  } else if (manual && status === 'checking') {
    text = 'Checking for updates…';
  } else if (manual && status === 'latest') {
    text = `You're up to date${version ? ` — ${version}` : ''}.`;
  } else if (manual && status === 'failed') {
    text = `Couldn't check for updates${error ? `: ${error}` : '.'}`;
    actions = (
      <>
        <Button size="sm" variant="outline" onClick={() => void check(true)}>
          Retry
        </Button>
        <Button size="sm" variant="ghost" onClick={dismiss}>
          Close
        </Button>
      </>
    );
  } else {
    return null;
  }

  return (
    <div className="fixed right-4 bottom-4 z-50 flex max-w-md items-center gap-3 rounded-lg border bg-popover px-4 py-3 text-sm text-popover-foreground shadow-lg">
      <span className="min-w-0 break-words">{text}</span>
      {actions}
    </div>
  );
}
