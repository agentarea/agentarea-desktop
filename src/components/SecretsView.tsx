import { useCallback, useEffect, useState } from 'react';
import { Eye, EyeOff, KeyRound, Loader2, Pencil, Plus, Trash2 } from 'lucide-react';
import { Input } from '@/components/ui/input';
import { cn } from '@/lib/utils';
import { createSecret, deleteSecret, listSecrets, parseHosts, updateSecret, type Secret } from '@/lib/vault';

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/**
 * Secrets for local agents. Values go straight to the OS keychain and are never
 * shown again (replace, don't reveal); the form's value field is the only place
 * one exists in JS, and it is cleared on save.
 */
export function SecretsView() {
  const [secrets, setSecrets] = useState<Secret[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      setSecrets(await listSecrets());
      setError(null);
    } catch (e) {
      setError(errorText(e));
      setSecrets([]);
    }
  }, []);
  useEffect(() => {
    void reload();
  }, [reload]);

  return (
    <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek">
      <div className="mx-auto flex max-w-3xl flex-col gap-6 px-6 pt-2 pb-10">
        <div className="flex items-start justify-between gap-6">
          <p className="text-[13px] leading-relaxed text-muted-foreground">
            Kept in your system keychain. Agents never see a value: they get a one-time handle that only works for the
            hosts you allow, and the value shows up as <code className="font-mono text-[12px]">[secret:NAME]</code> in
            anything they read.
          </p>
          {!adding && (
            <button
              onClick={() => setAdding(true)}
              className="flex shrink-0 items-center gap-1 rounded-full bg-foreground px-3 py-1 text-[12px] font-medium text-background"
            >
              <Plus className="size-3.5" /> Add secret
            </button>
          )}
        </div>

        {adding && (
          <div className="rounded-2xl border border-border px-4 py-3.5">
            <SecretForm
              onDone={() => {
                setAdding(false);
                void reload();
              }}
              onCancel={() => setAdding(false)}
            />
          </div>
        )}

        <section>
          <h2 className="mb-1.5 flex items-center gap-1.5 px-1 text-[12px] font-medium text-muted-foreground">
            On this device
            {!!secrets?.length && (
              <span className="rounded-full bg-secondary px-1.5 text-[11px] text-foreground">{secrets.length}</span>
            )}
          </h2>
          <div className="flex flex-col overflow-hidden rounded-2xl border border-border">
            {secrets === null && <p className="px-3.5 py-3 text-[12.5px] text-muted-foreground">Loading…</p>}
            {secrets?.length === 0 && (
              <p className="px-3.5 py-3 text-[12.5px] text-muted-foreground">
                No secrets yet. Add an API key and local agents can call that API without ever seeing the key.
              </p>
            )}
            {secrets?.map((s) =>
              editing === s.id ? (
                <div key={s.id} className="border-b border-border px-3.5 py-3 last:border-b-0">
                  <SecretForm
                    secret={s}
                    onDone={() => {
                      setEditing(null);
                      void reload();
                    }}
                    onCancel={() => setEditing(null)}
                  />
                </div>
              ) : (
                <SecretRow key={s.id} secret={s} onEdit={() => setEditing(s.id)} onDeleted={() => void reload()} />
              ),
            )}
          </div>
        </section>

        {error && <p className="px-1 text-[12px] text-destructive">{error}</p>}
      </div>
    </div>
  );
}

function SecretRow({ secret, onEdit, onDeleted }: { secret: Secret; onEdit: () => void; onDeleted: () => void }) {
  const [confirming, setConfirming] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const remove = async () => {
    try {
      await deleteSecret(secret.id);
      onDeleted();
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <div className="group flex flex-col gap-1 border-b border-border px-3.5 py-2.5 last:border-b-0 hover:bg-accent/50">
      <div className="flex min-w-0 items-center gap-2">
        <KeyRound className="size-4 shrink-0 text-muted-foreground" />
        <span className="shrink-0 font-mono text-[13px] font-medium">{secret.name}</span>
        {secret.description && (
          <span className="min-w-0 truncate text-[12.5px] text-muted-foreground">{secret.description}</span>
        )}
        <span className="ml-auto shrink-0 text-[11.5px] text-muted-foreground-subtle">
          {secret.lastUsedAt ? `Used ${ago(secret.lastUsedAt)}` : 'Never used'}
        </span>
        {confirming ? (
          <div className="flex shrink-0 items-center gap-1">
            <button
              onClick={() => void remove()}
              className="rounded-full bg-destructive px-2.5 py-0.5 text-[11.5px] font-medium text-white"
            >
              Delete
            </button>
            <button
              onClick={() => setConfirming(false)}
              className="rounded-full border border-border px-2.5 py-0.5 text-[11.5px] hover:bg-accent"
            >
              Keep
            </button>
          </div>
        ) : (
          <div className="flex shrink-0 items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
            <button
              onClick={onEdit}
              aria-label={`Edit ${secret.name}`}
              className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              <Pencil className="size-3.5" />
            </button>
            <button
              onClick={() => setConfirming(true)}
              aria-label={`Delete ${secret.name}`}
              className="rounded-md p-1 text-muted-foreground hover:bg-accent hover:text-destructive"
            >
              <Trash2 className="size-3.5" />
            </button>
          </div>
        )}
      </div>
      <div className="flex min-w-0 flex-wrap items-center gap-1 pl-6">
        {secret.allowedHosts.map((h) => (
          <span key={h} className="rounded-full bg-secondary px-2 py-0.5 font-mono text-[11px] text-foreground/80">
            {h}
          </span>
        ))}
        {secret.allowedHosts.length === 0 && (
          <span className="text-[11.5px] text-amber-600">No allowed hosts — agents can’t send it anywhere yet</span>
        )}
      </div>
      {secret.lastPurpose && (
        <p className="truncate pl-6 text-[11.5px] text-muted-foreground-subtle">Last used for: {secret.lastPurpose}</p>
      )}
      {error && <p className="pl-6 text-[12px] text-destructive">{error}</p>}
    </div>
  );
}

const inputCls = 'h-8 text-[13px]';

/** Add a secret, or edit one (`secret` given): a blank value keeps the stored one. */
function SecretForm({ secret, onDone, onCancel }: { secret?: Secret; onDone: () => void; onCancel: () => void }) {
  const [name, setName] = useState('');
  const [value, setValue] = useState('');
  const [show, setShow] = useState(false);
  const [description, setDescription] = useState(secret?.description ?? '');
  const [hosts, setHosts] = useState(secret?.allowedHosts.join(', ') ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const editing = !!secret;
  const ready = editing || (name.trim() !== '' && value !== '');

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!ready) return;
    setBusy(true);
    setError(null);
    try {
      if (secret) {
        await updateSecret(secret.id, {
          description,
          allowedHosts: parseHosts(hosts),
          value: value || undefined,
        });
      } else {
        await createSecret({ name, description: description || undefined, allowedHosts: parseHosts(hosts), value });
      }
      setValue('');
      onDone();
    } catch (err) {
      setError(errorText(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="flex flex-col gap-2.5">
      {editing ? (
        <div className="flex items-center gap-2">
          <KeyRound className="size-4 text-muted-foreground" />
          <span className="font-mono text-[13px] font-medium">{secret.name}</span>
        </div>
      ) : (
        <h3 className="text-[13.5px] font-medium">New secret</h3>
      )}
      <div className="grid grid-cols-[110px_1fr] items-center gap-x-3 gap-y-2">
        {!editing && (
          <>
            <label htmlFor="secret-name" className="text-[12px] text-muted-foreground">
              Name
            </label>
            <Input
              id="secret-name"
              autoFocus
              value={name}
              // Names are UPPER_SNAKE: fix it while typing instead of rejecting it.
              onChange={(e) => setName(e.target.value.toUpperCase().replace(/[^A-Z0-9_]/g, '_'))}
              placeholder="GITHUB_TOKEN"
              className={cn(inputCls, 'font-mono')}
            />
          </>
        )}
        <label htmlFor="secret-value" className="text-[12px] text-muted-foreground">
          Value
        </label>
        <div className="relative">
          <Input
            id="secret-value"
            type={show ? 'text' : 'password'}
            autoComplete="off"
            spellCheck={false}
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder={editing ? 'New value — leave empty to keep the current one' : 'Paste the secret'}
            className={cn(inputCls, 'pr-8 font-mono')}
          />
          <button
            type="button"
            onClick={() => setShow(!show)}
            aria-label={show ? 'Hide value' : 'Show value'}
            className="absolute top-1/2 right-2 -translate-y-1/2 text-muted-foreground hover:text-foreground"
          >
            {show ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
          </button>
        </div>
        <label htmlFor="secret-description" className="text-[12px] text-muted-foreground">
          Description
        </label>
        <Input
          id="secret-description"
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder="What it is for (agents see this)"
          className={inputCls}
        />
        <label htmlFor="secret-hosts" className="text-[12px] text-muted-foreground">
          Allowed hosts
        </label>
        <Input
          id="secret-hosts"
          value={hosts}
          onChange={(e) => setHosts(e.target.value)}
          placeholder="api.github.com, *.example.com"
          className={cn(inputCls, 'font-mono')}
        />
        <span />
        <span className="-mt-1 text-[11.5px] text-muted-foreground-subtle">
          The agent can only send this secret to these hosts, over https.
        </span>
      </div>
      <div className="flex items-center gap-2">
        <button
          type="submit"
          disabled={!ready || busy}
          className="flex items-center gap-1 rounded-full bg-foreground px-3 py-1 text-[12px] font-medium text-background disabled:opacity-50"
        >
          {busy && <Loader2 className="size-3.5 animate-spin" />}
          {editing ? 'Save' : 'Add secret'}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-full border border-border px-3 py-1 text-[12px] text-foreground hover:bg-accent"
        >
          Cancel
        </button>
        {error && <span className="min-w-0 truncate text-[12px] text-destructive">{error}</span>}
      </div>
    </form>
  );
}
