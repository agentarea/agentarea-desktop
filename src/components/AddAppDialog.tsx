/**
 * "Add app": install an MCP server to run on this device, from the bundled
 * Recommended list, the public MCP registry, or your own registry / a command.
 * Installing writes `~/AgentArea/mcp/servers.json` and starts the server.
 */
import { useEffect, useState, type FormEvent, type ReactNode } from 'react';
import { Dialog } from 'radix-ui';
import { Check, Loader2, Search, Trash2, X } from 'lucide-react';
import { EntityIcon } from '@/components/EntityIcon';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { useLocalMcp, type LocalServerSpec } from '@/lib/localMcp';
import {
  PUBLIC_REGISTRY,
  RECOMMENDED,
  installPlan,
  saveRegistries,
  savedRegistries,
  searchRegistry,
  slugId,
  specFromRegistry,
  splitArgs,
  type RegistryServer,
} from '@/lib/registry';
import { cn } from '@/lib/utils';

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function AddAppDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      {/* No portal: the dark theme is a class on the app wrapper, not on <body>. */}
      <Dialog.Overlay className="fixed inset-0 z-50 bg-black/25" />
      <Dialog.Content
        aria-describedby={undefined}
        className="fixed top-1/2 left-1/2 z-50 flex h-[min(640px,86vh)] w-[min(720px,92vw)] -translate-x-1/2 -translate-y-1/2 flex-col overflow-hidden rounded-xl border border-border bg-background text-foreground shadow-xl"
      >
        <div className="flex items-center gap-2 px-5 pt-4 pb-2">
          <Dialog.Title className="flex-1 text-[15px] font-medium">Add an app to this device</Dialog.Title>
          <Dialog.Close asChild>
            <Button variant="ghost" size="icon-xs" aria-label="Close">
              <X />
            </Button>
          </Dialog.Close>
        </div>
        <Tabs defaultValue="recommended" className="min-h-0 flex-1 gap-0">
          <div className="px-5 pb-3">
            <TabsList>
              <TabsTrigger value="recommended">Recommended</TabsTrigger>
              <TabsTrigger value="public">Public registry</TabsTrigger>
              <TabsTrigger value="custom">Custom</TabsTrigger>
            </TabsList>
          </div>
          <TabsContent value="recommended" className="min-h-0 border-t border-border">
            <Recommended />
          </TabsContent>
          <TabsContent value="public" className="flex min-h-0 flex-col border-t border-border">
            <RegistrySearch base={PUBLIC_REGISTRY} />
          </TabsContent>
          <TabsContent value="custom" className="min-h-0 border-t border-border">
            <Custom />
          </TabsContent>
        </Tabs>
      </Dialog.Content>
    </Dialog.Root>
  );
}

/** One installable entry: icon, name, description, and its install control. */
function Row({
  name,
  icons,
  seed,
  description,
  meta,
  action,
  children,
}: {
  name: string;
  icons?: string[];
  seed: string;
  description?: string;
  meta?: ReactNode;
  action: ReactNode;
  children?: ReactNode;
}) {
  return (
    <div className="border-b border-border px-5 py-3 last:border-b-0">
      <div className="flex items-start gap-3">
        <EntityIcon name={name} icons={icons} seed={seed} className="mt-0.5 size-7 rounded-lg text-[10px]" />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-medium">{name}</span>
            {meta}
          </div>
          {description && <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">{description}</p>}
        </div>
        <div className="shrink-0">{action}</div>
      </div>
      {children}
    </div>
  );
}

/** Install button with its own busy/error state. */
function useInstall() {
  const install = useLocalMcp((s) => s.install);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<{ id: string; message: string } | null>(null);
  const run = async (spec: LocalServerSpec): Promise<boolean> => {
    setBusy(spec.id);
    setError(null);
    try {
      await install(spec);
      return true;
    } catch (e) {
      setError({ id: spec.id, message: errorText(e) });
      return false;
    } finally {
      setBusy(null);
    }
  };
  return { busy, error, run };
}

function InstallButton({ installed, busy, onClick, label = 'Install' }: { installed: boolean; busy: boolean; onClick: () => void; label?: string }) {
  if (installed)
    return (
      <Button size="xs" variant="ghost" disabled>
        <Check /> Installed
      </Button>
    );
  return (
    <Button size="xs" variant="outline" disabled={busy} onClick={onClick}>
      {busy && <Loader2 className="animate-spin" />}
      {label}
    </Button>
  );
}

function ErrorLine({ children }: { children: ReactNode }) {
  return <p className="mt-2 break-words text-xs text-destructive">{children}</p>;
}

function Recommended() {
  const installed = useLocalMcp((s) => s.servers);
  const { busy, error, run } = useInstall();
  return (
    <ScrollArea className="h-full">
      <p className="px-5 pt-3 text-xs text-muted-foreground">
        Example MCP Apps from the Model Context Protocol project. They run here via <code>npx</code> — Node.js required.
      </p>
      {RECOMMENDED.map((spec) => (
        <Row
          key={spec.id}
          name={spec.name}
          seed={spec.id}
          description={spec.description}
          meta={<span className="truncate text-[11px] text-muted-foreground-subtle">{spec.args[1]}</span>}
          action={<InstallButton installed={installed.some((s) => s.id === spec.id)} busy={busy === spec.id} onClick={() => void run(spec)} />}
        >
          {error?.id === spec.id && <ErrorLine>{error.message}</ErrorLine>}
        </Row>
      ))}
    </ScrollArea>
  );
}

/** Search one `/v0/servers` registry and install stdio packages from it. */
function RegistrySearch({ base }: { base: string }) {
  const installed = useLocalMcp((s) => s.servers);
  const { busy, error, run } = useInstall();
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<RegistryServer[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);
  /** entry whose inputs (env vars, required args) are being filled in */
  const [expanded, setExpanded] = useState<string | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});

  const search = async (q: string) => {
    setLoading(true);
    setSearchError(null);
    try {
      setResults(await searchRegistry(base, q));
    } catch (e) {
      setSearchError(errorText(e));
      setResults(null);
    } finally {
      setLoading(false);
    }
  };

  // Show the registry's first page right away.
  useEffect(() => {
    setResults(null);
    setExpanded(null);
    void search('');
  }, [base]);

  const onSubmit = (e: FormEvent) => {
    e.preventDefault();
    void search(query);
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <form onSubmit={onSubmit} className="flex gap-2 px-5 py-3">
        <div className="relative flex-1">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search servers" className="h-8 pl-8" />
        </div>
        <Button type="submit" size="sm" variant="outline" disabled={loading}>
          {loading && <Loader2 className="animate-spin" />}
          Search
        </Button>
      </form>
      <ScrollArea className="min-h-0 flex-1 border-t border-border">
        {searchError && <p className="px-5 py-3 text-xs text-destructive">{searchError}</p>}
        {results?.length === 0 && <p className="px-5 py-3 text-xs text-muted-foreground">Nothing found.</p>}
        {results?.map((server) => {
          const plan = installPlan(server);
          const id = slugId(server.name);
          const icons = (server.icons ?? []).map((i) => i.src).filter((s): s is string => !!s);
          const open = expanded === server.name && plan.kind === 'package';
          const missing = plan.kind === 'package' && plan.inputs.some((i) => i.required && !(values[i.key] ?? i.default));
          const install = () => {
            if (plan.kind !== 'package') return;
            if (plan.inputs.length && !open) {
              setExpanded(server.name);
              setValues({});
              return;
            }
            void run(specFromRegistry(server, plan, values, base));
          };
          return (
            <Row
              key={server.name}
              name={server.title || server.name}
              icons={icons}
              seed={server.name}
              description={server.description}
              meta={
                <>
                  {server.version && <span className="text-[11px] text-muted-foreground-subtle">v{server.version}</span>}
                  {plan.kind === 'package' && (
                    <Badge variant="outline" className="px-1.5 py-0 text-[10px] font-normal">
                      {plan.pkg.registryType}
                    </Badge>
                  )}
                </>
              }
              action={
                plan.kind === 'package' ? (
                  <InstallButton
                    installed={installed.some((s) => s.id === id)}
                    busy={busy === id}
                    onClick={install}
                    label={plan.inputs.length && !open ? 'Set up…' : 'Install'}
                  />
                ) : (
                  <Button size="xs" variant="ghost" disabled>
                    {plan.kind === 'remote' ? 'Remote only' : 'Not installable'}
                  </Button>
                )
              }
            >
              {open && plan.kind === 'package' && (
                <div className="mt-3 ml-10 flex flex-col gap-2">
                  {plan.inputs.map((input) => (
                    <label key={input.key} className="flex flex-col gap-1">
                      <span className="text-xs font-medium">
                        {input.label}
                        {input.required && <span className="text-destructive"> *</span>}
                      </span>
                      <Input
                        type={input.secret ? 'password' : 'text'}
                        className="h-8 font-mono text-xs"
                        placeholder={input.default ?? ''}
                        value={values[input.key] ?? ''}
                        onChange={(e) => setValues((v) => ({ ...v, [input.key]: e.target.value }))}
                      />
                      {input.description && <span className="text-[11px] text-muted-foreground">{input.description}</span>}
                    </label>
                  ))}
                  <div className="flex items-center gap-2">
                    <Button size="xs" disabled={missing || busy === id} onClick={install}>
                      {busy === id && <Loader2 className="animate-spin" />}
                      Install
                    </Button>
                    <Button size="xs" variant="ghost" onClick={() => setExpanded(null)}>
                      Cancel
                    </Button>
                    <span className="text-[11px] text-muted-foreground-subtle">Stored in ~/AgentArea/mcp/servers.json</span>
                  </div>
                </div>
              )}
              {error?.id === id && <ErrorLine>{error.message}</ErrorLine>}
            </Row>
          );
        })}
      </ScrollArea>
    </div>
  );
}

/** Your own registries (same API as the public one) and manual commands. */
function Custom() {
  const [registries, setRegistries] = useState<string[]>(savedRegistries);
  const [active, setActive] = useState<string | null>(null);
  const [url, setUrl] = useState('');
  const [urlError, setUrlError] = useState<string | null>(null);

  const addRegistry = (e: FormEvent) => {
    e.preventDefault();
    let base: string;
    try {
      const parsed = new URL(url.trim());
      if (parsed.protocol !== 'https:') throw new Error('Registry URLs must use https');
      base = parsed.href.replace(/\/+$/, '').replace(/\/v0\/servers$/, '');
    } catch (err) {
      setUrlError(err instanceof TypeError ? 'Not a valid URL' : errorText(err));
      return;
    }
    const next = [base, ...registries.filter((r) => r !== base)];
    setRegistries(next);
    saveRegistries(next);
    setUrl('');
    setUrlError(null);
    setActive(base);
  };

  const removeRegistry = (base: string) => {
    const next = registries.filter((r) => r !== base);
    setRegistries(next);
    saveRegistries(next);
    if (active === base) setActive(null);
  };

  if (active) {
    return (
      <div className="flex h-full min-h-0 flex-col">
        <div className="flex items-center gap-2 px-5 pt-3 text-xs">
          <Button size="xs" variant="ghost" onClick={() => setActive(null)}>
            ← Back
          </Button>
          <span className="truncate text-muted-foreground">{active}</span>
        </div>
        <RegistrySearch base={active} />
      </div>
    );
  }

  return (
    <ScrollArea className="h-full">
      <section className="px-5 py-4">
        <h3 className="text-sm font-medium">Your registries</h3>
        <p className="mt-0.5 text-xs text-muted-foreground">
          Any registry that implements the MCP registry API (<code>/v0/servers</code>) — for example your company’s.
        </p>
        <form onSubmit={addRegistry} className="mt-3 flex gap-2">
          <Input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://registry.example.com" className="h-8" />
          <Button type="submit" size="sm" variant="outline" disabled={!url.trim()}>
            Add
          </Button>
        </form>
        {urlError && <ErrorLine>{urlError}</ErrorLine>}
        {registries.length > 0 && (
          <div className="mt-3 flex flex-col rounded-lg border border-border">
            {registries.map((base) => (
              <div key={base} className="flex items-center gap-2 border-b border-border px-3 py-2 last:border-b-0">
                <button type="button" onClick={() => setActive(base)} className="min-w-0 flex-1 cursor-pointer truncate text-left text-sm hover:underline">
                  {base}
                </button>
                <Button size="icon-xs" variant="ghost" aria-label={`Remove ${base}`} onClick={() => removeRegistry(base)}>
                  <Trash2 />
                </Button>
              </div>
            ))}
          </div>
        )}
      </section>
      <ManualForm />
    </ScrollArea>
  );
}

/** Any MCP server: a stdio command line, or the URL of one already running. */
function ManualForm() {
  const installed = useLocalMcp((s) => s.servers);
  const { busy, error, run } = useInstall();
  const [name, setName] = useState('');
  const [command, setCommand] = useState('');
  const [env, setEnv] = useState('');
  const [formError, setFormError] = useState<string | null>(null);
  const id = slugId(name);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    const input = command.trim();
    // A URL means a server that is already running (streamable HTTP).
    const url = /^https?:\/\//i.test(input) ? input : null;
    const [cmd, ...args] = url ? [''] : splitArgs(input);
    const pairs = env
      .split('\n')
      .map((l) => l.trim())
      .filter(Boolean)
      .map((l) => (l.includes('=') ? ([l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)] as const) : ([l, null] as const)));
    if (!cmd && !url) return setFormError('Enter the command that starts the server, or its URL');
    if (pairs.some(([k, v]) => v === null || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(k))) return setFormError('Environment lines must look like KEY=value');
    if (installed.some((s) => s.id === id)) return setFormError(`“${name}” is already installed`);
    setFormError(null);
    void run({
      id,
      name: name.trim(),
      source: 'custom',
      command: cmd,
      args,
      ...(url ? { url } : {}),
      env: pairs.length ? Object.fromEntries(pairs.map(([k, v]) => [k, v ?? ''])) : undefined,
    }).then((ok) => {
      if (!ok) return;
      setName('');
      setCommand('');
      setEnv('');
    });
  };

  return (
    <section className="border-t border-border px-5 py-4">
      <h3 className="text-sm font-medium">Command or URL</h3>
      <p className="mt-0.5 text-xs text-muted-foreground">
        A stdio command the app runs, or the URL of a server already running (streamable HTTP, e.g. http://localhost:3402/mcp).
      </p>
      <form onSubmit={submit} className="mt-3 flex flex-col gap-2">
        <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="Name" className="h-8" required />
        <Input
          value={command}
          onChange={(e) => setCommand(e.target.value)}
          placeholder="npx -y @modelcontextprotocol/server-everything   or   http://localhost:3402/mcp"
          className="h-8 font-mono text-xs"
          required
        />
        <textarea
          value={env}
          onChange={(e) => setEnv(e.target.value)}
          placeholder={'Environment, one per line: API_KEY=…'}
          rows={3}
          className={cn(
            'w-full resize-none rounded-md border border-input bg-transparent px-3 py-2 font-mono text-xs shadow-xs outline-none',
            'placeholder:text-muted-foreground/60 focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50',
          )}
        />
        <div>
          <Button type="submit" size="sm" disabled={!name.trim() || !command.trim() || busy === id}>
            {busy === id && <Loader2 className="animate-spin" />}
            Install
          </Button>
        </div>
        {(formError || error) && <ErrorLine>{formError ?? error?.message}</ErrorLine>}
      </form>
    </section>
  );
}
