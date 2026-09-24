import { ArrowRight, Loader2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { useAppStore } from '@/store';
import { ENVIRONMENTS } from '@/lib/config';
import logoIcon from '@/assets/logo-icon.svg';

/**
 * Login gate. "Continue with AgentArea" opens the system browser to Ory/Kratos
 * (the real web sign-in), then the desktop captures the OAuth loopback callback.
 */
export function LoginScreen() {
  const status = useAppStore((s) => s.auth.status);
  const error = useAppStore((s) => s.auth.error);
  const login = useAppStore((s) => s.login);
  const devComplete = useAppStore((s) => s.devComplete);
  const busy = status === 'authenticating';

  return (
    <div className="flex h-full flex-col bg-background text-foreground">
      <div data-tauri-drag-region className="h-12 shrink-0" />
      <div className="flex flex-1 items-center justify-center px-6">
        <div className="w-full max-w-sm text-center">
          <img src={logoIcon} alt="AgentArea" className="mx-auto mb-5 size-14 rounded-2xl shadow-sm" />
          <h1 className="text-xl font-semibold tracking-tight">Sign in to AgentArea</h1>
          <p className="mt-1.5 text-sm text-muted-foreground">
            Connect this device to your workspace to run and watch your agents.
          </p>

          <EnvPicker disabled={busy} />

          <Button onClick={login} disabled={busy} className="mt-5 w-full gap-2 rounded-full">
            {busy ? (
              <>
                <Loader2 className="size-4 animate-spin" /> Waiting for browser…
              </>
            ) : (
              <>
                Continue with AgentArea <ArrowRight className="size-4" />
              </>
            )}
          </Button>

          {busy && (
            <p className="mt-3 text-xs text-muted-foreground">
              Finish signing in in your browser, then return here.
            </p>
          )}

          {error && <p className="mt-3 text-xs text-destructive">{error}</p>}

          {/* Dev-only shortcut until you want to always go through the browser. */}
          <button
            onClick={devComplete}
            className="mt-6 text-xs text-muted-foreground/70 underline-offset-4 hover:underline"
          >
            Skip (dev preview)
          </button>
        </div>
      </div>
    </div>
  );
}

/** Quiet segmented control for the target deployment. Switching here logs out (see store.setEnv); it's not exposed once signed in. */
function EnvPicker({ disabled }: { disabled?: boolean }) {
  const env = useAppStore((s) => s.env);
  const setEnv = useAppStore((s) => s.setEnv);

  return (
    <div
      role="tablist"
      aria-label="Environment"
      className="mx-auto mt-5 inline-flex items-center gap-0.5 rounded-full border border-border bg-secondary p-0.5"
    >
      {ENVIRONMENTS.map((e) => (
        <button
          key={e.id}
          role="tab"
          aria-selected={env.id === e.id}
          disabled={disabled}
          onClick={() => setEnv(e.id)}
          className={cn(
            'rounded-full px-3 py-1 text-[12px] font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-60',
            env.id === e.id
              ? 'bg-background text-foreground shadow-[0_1px_4px_rgba(0,0,0,0.08)]'
              : 'text-muted-foreground hover:text-foreground',
          )}
        >
          {e.label}
        </button>
      ))}
    </div>
  );
}
