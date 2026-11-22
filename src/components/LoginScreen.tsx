import { useState } from 'react';
import { ArrowRight, Loader2 } from 'lucide-react';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { useAppStore } from '@/store';
import { parseServerUrl, SERVERS } from '@/lib/config';
import logoIcon from '@/assets/logo-icon.svg';

/**
 * Login gate. "Continue with AgentArea" opens the system browser to Ory/Kratos
 * (the real web sign-in), then the desktop captures the OAuth loopback callback.
 */
export function LoginScreen() {
  const status = useAppStore((s) => s.auth.status);
  const error = useAppStore((s) => s.auth.error);
  const login = useAppStore((s) => s.login);
  const continueWithoutSignIn = useAppStore((s) => s.continueWithoutSignIn);
  const retrySignIn = useAppStore((s) => s.retrySignIn);
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

          <ServerPicker disabled={busy} />

          {busy && (
            <p className="mt-3 text-xs text-muted-foreground">
              Finish signing in in your browser, then return here.
            </p>
          )}

          {error && (
            <div className="mt-3 text-xs text-destructive">
              <p>{error}</p>
              {/Keychain/.test(error) && (
                <Button variant="outline" size="sm" onClick={retrySignIn} disabled={busy} className="mt-2 rounded-full">
                  Try again
                </Button>
              )}
            </div>
          )}

        </div>
      </div>
      <div className="shrink-0 pb-6 text-center">
        <button
          onClick={continueWithoutSignIn}
          disabled={busy}
          className="text-xs text-muted-foreground/70 underline-offset-4 hover:underline disabled:opacity-50"
        >
          Continue without signing in
        </button>
      </div>
    </div>
  );
}

/**
 * Which AgentArea server to sign in to: agentarea.ai unless changed, another
 * known one, or a self-hosted URL. Switching logs out (see store.setEnv); it is
 * not exposed once signed in.
 */
function ServerPicker({ disabled }: { disabled?: boolean }) {
  const env = useAppStore((s) => s.env);
  const setEnv = useAppStore((s) => s.setEnv);
  const [open, setOpen] = useState(false);
  const [url, setUrl] = useState('');
  const [invalid, setInvalid] = useState(false);

  const choose = (apiBaseUrl: string) => {
    setEnv(apiBaseUrl);
    setOpen(false);
    setUrl('');
    setInvalid(false);
  };

  if (!open) {
    return (
      <p className="mt-3 text-xs text-muted-foreground">
        Server: <span className="text-foreground">{env.label}</span> ·{' '}
        <button
          disabled={disabled}
          onClick={() => setOpen(true)}
          className="underline-offset-4 hover:text-foreground hover:underline disabled:opacity-50"
        >
          Change
        </button>
      </p>
    );
  }

  return (
    <div className="mt-4 rounded-xl border border-border p-3 text-left">
      <div className="flex flex-wrap gap-1.5">
        {SERVERS.map((s) => (
          <button
            key={s.id}
            disabled={disabled}
            onClick={() => choose(s.apiBaseUrl)}
            className={cn(
              'rounded-full border px-3 py-1 text-[12px] font-medium transition-colors disabled:opacity-60',
              env.apiBaseUrl === s.apiBaseUrl
                ? 'border-foreground/20 bg-secondary text-foreground'
                : 'border-border text-muted-foreground hover:text-foreground',
            )}
          >
            {s.label}
          </button>
        ))}
      </div>
      <form
        className="mt-2.5 flex gap-1.5"
        onSubmit={(e) => {
          e.preventDefault();
          const apiBaseUrl = parseServerUrl(url);
          if (apiBaseUrl) choose(apiBaseUrl);
          else setInvalid(true);
        }}
      >
        <Input
          value={url}
          onChange={(e) => {
            setUrl(e.target.value);
            setInvalid(false);
          }}
          placeholder="Your server's API, e.g. api.example.com"
          aria-invalid={invalid || undefined}
          disabled={disabled}
          className="h-8 text-[12px]"
        />
        <Button type="submit" size="sm" variant="outline" disabled={disabled || !url.trim()}>
          Use
        </Button>
      </form>
      {invalid && <p className="mt-1.5 text-[11px] text-destructive">Enter an https address of an AgentArea API.</p>}
    </div>
  );
}
