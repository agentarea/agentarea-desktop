/**
 * Self-contained panels for MCP Apps (local ones from servers this device runs,
 * cloud ones from the AgentArea workspace) and local plugins (from
 * ~/AgentArea/plugins). Each is a list on the left and the selected item
 * rendered on the right; mount them wherever there is room.
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { AppWindow, Cloud, Laptop, Play, Plus, Puzzle, RefreshCw, Square, Trash2 } from 'lucide-react';
import { AddAppDialog } from '@/components/AddAppDialog';
import { EntityIcon } from '@/components/EntityIcon';
import { AppView } from '@/components/AppView';
import type { AppRef } from '@/lib/pinnedApps';
import { PluginFrame } from '@/components/PluginFrame';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { ScrollArea } from '@/components/ui/scroll-area';
import { listMcpApps, type McpApp } from '@/lib/api';
import {
  appsOf,
  serverState,
  useLocalMcp,
  type LocalApp,
  type LocalServer,
  type ServerState,
} from '@/lib/localMcp';
import { listPlugins, setPluginEnabled, usePlugins, type Plugin } from '@/lib/plugins';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Centered note for empty, error and not-connected states. */
function Note({ title, children, error }: { title: string; children?: ReactNode; error?: boolean }) {
  return (
    <div className="flex flex-1 items-center justify-center p-6">
      <div
        role={error ? 'alert' : undefined}
        className={cn(
          'max-w-md rounded-xl border p-5 text-center text-sm',
          error ? 'border-destructive/30 bg-destructive/5' : 'border-border bg-muted/40',
        )}
      >
        <p className={cn('font-medium', error && 'text-destructive')}>{title}</p>
        {children && <p className="mt-1.5 break-words text-muted-foreground">{children}</p>}
      </div>
    </div>
  );
}

interface ListItem {
  key: string;
  title: string;
  subtitle?: string | null;
  /** greyed out (e.g. a switched-off plugin) */
  muted?: boolean;
  /** control shown at the row's right edge */
  trailing?: React.ReactNode;
}

/** Shared two-pane shell: item list with a refresh button, content beside it. */
function ListPane({
  heading,
  icon,
  items,
  selected,
  onSelect,
  onRefresh,
  loading,
  empty,
  children,
}: {
  heading: string;
  icon: ReactNode;
  items: ListItem[];
  selected: string | null;
  onSelect: (key: string) => void;
  onRefresh: () => void;
  loading: boolean;
  empty: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex h-full min-h-0 flex-1 bg-background">
      <div className="flex w-64 shrink-0 flex-col border-r border-border">
        <div className="flex h-11 items-center gap-2 px-3 text-sm font-medium">
          {icon}
          <span className="flex-1">{heading}</span>
          <Button variant="ghost" size="icon-xs" onClick={onRefresh} disabled={loading} aria-label="Refresh">
            <RefreshCw className={cn(loading && 'animate-spin')} />
          </Button>
        </div>
        <ScrollArea className="min-h-0 flex-1">
          <div className="flex flex-col gap-0.5 px-2 pb-2">
            {items.length === 0 && !loading && <div className="px-2 py-3 text-xs text-muted-foreground">{empty}</div>}
            {items.map((item) => (
              <div
                key={item.key}
                className={cn(
                  'flex items-center gap-2 rounded-lg pr-2 transition-colors hover:bg-accent',
                  selected === item.key && 'bg-accent',
                )}
              >
                <button
                  type="button"
                  onClick={() => onSelect(item.key)}
                  className={cn('flex min-w-0 flex-1 cursor-pointer flex-col px-2.5 py-2 text-left', item.muted && 'opacity-50')}
                >
                  <span className="truncate text-sm">{item.title}</span>
                  {item.subtitle && <span className="truncate text-xs text-muted-foreground">{item.subtitle}</span>}
                </button>
                {item.trailing}
              </div>
            ))}
          </div>
        </ScrollArea>
      </div>
      <div className="flex min-w-0 flex-1 flex-col">{children}</div>
    </div>
  );
}

const cloudKey = (a: McpApp) => `cloud:${a.instance_id}/${a.tool_name}`;
const localKey = (a: LocalApp) => `local:${a.serverId}/${a.toolName}`;
const serverKey = (id: string) => `server:${id}`;


// Installed servers start once per app run when the panel opens; one the user
// stopped stays stopped.
const autoStarted = new Set<string>();

const STATE_LABEL: Record<ServerState, string> = {
  stopped: 'Stopped',
  starting: 'Starting…',
  running: 'Running',
  error: 'Failed',
};

/** Where an app runs, said explicitly on every row. */
function WhereBadge({ local }: { local: boolean }) {
  return (
    <Badge variant="outline" className="gap-1 px-1.5 py-0 text-[10px] font-normal text-muted-foreground">
      {local ? <Laptop className="size-2.5" /> : <Cloud className="size-2.5" />}
      {local ? 'Local' : 'Cloud'}
    </Badge>
  );
}

function StateDot({ state }: { state: ServerState }) {
  return (
    <span
      className={cn(
        'size-1.5 shrink-0 rounded-full',
        state === 'running' && 'bg-emerald-500',
        state === 'starting' && 'animate-pulse bg-amber-500',
        state === 'error' && 'bg-destructive',
        state === 'stopped' && 'bg-foreground/20',
      )}
    />
  );
}

function SectionHeader({ icon, title, action }: { icon: ReactNode; title: string; action?: ReactNode }) {
  return (
    <div className="flex h-8 items-center gap-1.5 px-2.5 text-[11px] font-medium text-muted-foreground">
      {icon}
      <span className="min-w-0 flex-1 truncate">{title}</span>
      {action}
    </div>
  );
}

function AppRow({
  title,
  subtitle,
  icons,
  seed,
  local,
  selected,
  onSelect,
}: {
  title: string;
  subtitle: string;
  icons?: string[];
  seed: string;
  local: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        'flex w-full cursor-pointer items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-left transition-colors hover:bg-accent',
        selected && 'bg-accent',
      )}
    >
      <EntityIcon name={title} icons={icons} seed={seed} className="size-6 rounded-md text-[9px]" />
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-sm">{title}</span>
        <span className="truncate text-xs text-muted-foreground">{subtitle}</span>
      </span>
      <WhereBadge local={local} />
    </button>
  );
}

function errorOf(server: LocalServer, errors: Record<string, string>): string | null {
  return errors[server.id] ?? (server.status === 'error' ? server.stderr || 'The server exited.' : null);
}

export interface AppsPanelProps {
  /** signed in and connected to the control plane; cloud apps come from the API */
  connected: boolean;
}

/**
 * MCP Apps in two explicit places: servers this device runs itself (no sign-in
 * needed) and the AgentArea workspace's MCP server instances (`GET /v1/mcp-apps/`).
 */
export function AppsPanel({ connected }: AppsPanelProps) {
  const [cloudApps, setCloudApps] = useState<McpApp[]>([]);
  const [cloudLoading, setCloudLoading] = useState(false);
  const [cloudError, setCloudError] = useState<string | null>(null);
  const workspace = useAppStore((s) => s.workspaces.find((w) => w.id === s.workspaceId)?.name ?? null);
  const connecting = useAppStore((s) => s.connection === 'connecting');
  const mcps = useAppStore((s) => s.mcps);

  const servers = useLocalMcp((s) => s.servers);
  const tools = useLocalMcp((s) => s.tools);
  const starting = useLocalMcp((s) => s.starting);
  const errors = useLocalMcp((s) => s.errors);
  const listError = useLocalMcp((s) => s.listError);

  const [selected, setSelected] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const loadCloud = useCallback(async () => {
    if (!connected) return;
    setCloudLoading(true);
    setCloudError(null);
    try {
      setCloudApps(await listMcpApps());
    } catch (e) {
      setCloudError(errorText(e));
    } finally {
      setCloudLoading(false);
    }
  }, [connected]);

  useEffect(() => {
    if (connected) void loadCloud();
    else setCloudApps([]);
  }, [connected, loadCloud]);

  // Start installed servers so their apps are there to click.
  useEffect(() => {
    const { reload, start } = useLocalMcp.getState();
    void reload().then(() => {
      for (const s of useLocalMcp.getState().servers) {
        if (s.status !== 'stopped' || autoStarted.has(s.id)) continue;
        autoStarted.add(s.id);
        void start(s.id);
      }
    });
  }, []);

  const localApps = servers.flatMap((s) => (tools[s.id] ? appsOf(s, tools[s.id]) : []));
  const localApp = localApps.find((a) => localKey(a) === selected) ?? null;
  const cloudApp = cloudApps.find((a) => cloudKey(a) === selected) ?? null;
  const server = servers.find((s) => serverKey(s.id) === selected) ?? null;

  // The selected app as a self-contained reference: AppView opens it, and the
  // same object is what gets pinned to the sidebar.
  const appRef: AppRef | null = localApp
    ? {
        key: localKey(localApp),
        kind: 'local',
        title: localApp.title,
        subtitle: localApp.serverName,
        icons: localApp.icons,
        seed: localKey(localApp),
        toolName: localApp.toolName,
        resourceUri: localApp.resourceUri,
        requiresInput: localApp.requiresInput,
        serverId: localApp.serverId,
      }
    : cloudApp
      ? {
          key: cloudKey(cloudApp),
          kind: 'cloud',
          title: cloudApp.title || cloudApp.tool_name,
          subtitle: cloudApp.instance_name,
          icons: mcps.find((m) => m.id === cloudApp.instance_id)?.icons,
          seed: cloudApp.instance_id,
          toolName: cloudApp.tool_name,
          resourceUri: cloudApp.resource_uri,
          requiresInput: cloudApp.requires_input,
          instanceId: cloudApp.instance_id,
        }
      : null;

  const refresh = () => {
    void useLocalMcp.getState().reload();
    void loadCloud();
  };


  return (
    <div className="flex h-full min-h-0 flex-1 bg-background">
      <div className="flex w-72 shrink-0 flex-col border-r border-border">
        <div className="flex h-11 items-center gap-2 px-3 text-sm font-medium">
          <AppWindow className="size-4 text-muted-foreground" />
          <span className="flex-1">Apps</span>
          <Button variant="ghost" size="icon-xs" onClick={refresh} disabled={cloudLoading} aria-label="Refresh">
            <RefreshCw className={cn(cloudLoading && 'animate-spin')} />
          </Button>
        </div>
        <ScrollArea className="min-h-0 flex-1">
          <div className="flex flex-col px-2 pb-3">
            <SectionHeader
              icon={<Laptop className="size-3.5" />}
              title="On this device"
              action={
                <Button variant="ghost" size="xs" onClick={() => setAdding(true)}>
                  <Plus /> Add app
                </Button>
              }
            />
            {listError && <p className="px-2.5 py-1 text-xs break-words text-destructive">{listError}</p>}
            {!listError && servers.length === 0 && (
              <p className="px-2.5 py-1 text-xs text-muted-foreground">Nothing installed yet. Add an app to run it on this device.</p>
            )}
            {servers.map((s) => {
              const state = serverState(s, !!starting[s.id], errors[s.id]);
              const apps = tools[s.id] ? appsOf(s, tools[s.id]) : [];
              return (
                <div key={s.id} className="flex flex-col">
                  <button
                    type="button"
                    onClick={() => setSelected(serverKey(s.id))}
                    title={`${s.name} — ${STATE_LABEL[state]}`}
                    className={cn(
                      'flex w-full cursor-pointer items-center gap-2 rounded-md px-2.5 py-1 text-left transition-colors hover:bg-accent',
                      selected === serverKey(s.id) && 'bg-accent',
                    )}
                  >
                    <StateDot state={state} />
                    <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">{s.name}</span>
                    {state === 'running' && tools[s.id] && apps.length === 0 && (
                      <span className="text-[10px] text-muted-foreground-subtle">No UI</span>
                    )}
                    {state !== 'running' && (
                      <span className={cn('text-[10px]', state === 'error' ? 'text-destructive' : 'text-muted-foreground-subtle')}>
                        {STATE_LABEL[state]}
                      </span>
                    )}
                  </button>
                  {apps.map((a) => (
                    <AppRow
                      key={localKey(a)}
                      title={a.title}
                      subtitle={a.serverName}
                      icons={a.icons}
                      seed={localKey(a)}
                      local
                      selected={selected === localKey(a)}
                      onSelect={() => setSelected(localKey(a))}
                    />
                  ))}
                </div>
              );
            })}

            <div className="mt-3">
              <SectionHeader icon={<Cloud className="size-3.5" />} title={workspace ? `AgentArea · ${workspace}` : 'AgentArea'} />
            </div>
            {!connected ? (
              <p className="px-2.5 py-1 text-xs text-muted-foreground">
                {connecting ? 'Connecting…' : 'Sign in to see workspace apps.'}
              </p>
            ) : cloudError ? (
              <p className="px-2.5 py-1 text-xs break-words text-destructive">Couldn’t load apps: {cloudError}</p>
            ) : cloudApps.length === 0 && !cloudLoading ? (
              <p className="px-2.5 py-1 text-xs text-muted-foreground">No apps yet. MCP servers that ship a UI show up here.</p>
            ) : (
              cloudApps.map((a) => (
                <AppRow
                  key={cloudKey(a)}
                  title={a.title || a.tool_name}
                  subtitle={a.instance_name}
                  icons={mcps.find((m) => m.id === a.instance_id)?.icons}
                  seed={a.instance_id}
                  local={false}
                  selected={selected === cloudKey(a)}
                  onSelect={() => setSelected(cloudKey(a))}
                />
              ))
            )}
          </div>
        </ScrollArea>
      </div>

      <div className="flex min-w-0 flex-1 flex-col">
        {server ? (
          <ServerDetail server={server} onStopped={() => autoStarted.add(server.id)} onRemoved={() => setSelected(null)} />
        ) : !appRef ? (
          <Note title="Pick an app">
            Apps are interactive views from MCP servers — running on this device, or in your AgentArea workspace.
          </Note>
        ) : (
          <AppView key={appRef.key} app={appRef} />
        )}
      </div>

      <AddAppDialog open={adding} onOpenChange={setAdding} />
    </div>
  );
}

/** A local server: what it runs, whether it is up (and why not), and its tools. */
function ServerDetail({ server, onStopped, onRemoved }: { server: LocalServer; onStopped: () => void; onRemoved: () => void }) {
  const tools = useLocalMcp((s) => s.tools[server.id]);
  const starting = useLocalMcp((s) => !!s.starting[server.id]);
  const errors = useLocalMcp((s) => s.errors);
  const { start, stop, uninstall } = useLocalMcp.getState();
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const state = serverState(server, starting, errors[server.id]);
  const error = errorOf(server, errors);
  const appNames = new Set(appsOf(server, tools ?? []).map((a) => a.toolName));

  const act = async (fn: () => Promise<void>) => {
    setBusy(true);
    setActionError(null);
    try {
      await fn();
    } catch (e) {
      setActionError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const source =
    server.source === 'recommended'
      ? 'Recommended'
      : server.source === 'registry'
        ? `Registry · ${server.registry ?? ''}`
        : server.url
          ? 'Custom URL'
          : 'Custom command';

  return (
    <>
      <div className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-2.5">
        <EntityIcon name={server.name} icons={server.icons} seed={server.id} className="size-7 rounded-lg text-[10px]" />
        <div className="min-w-0 flex-1">
          <h2 className="truncate text-sm font-medium">{server.name}</h2>
          <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <StateDot state={state} /> {STATE_LABEL[state]} · on this device
          </p>
        </div>
        {state === 'running' || state === 'starting' ? (
          <Button
            size="sm"
            variant="outline"
            disabled={busy || starting}
            onClick={() =>
              void act(async () => {
                onStopped();
                await stop(server.id);
              })
            }
          >
            <Square /> Stop
          </Button>
        ) : (
          <Button size="sm" variant="outline" disabled={busy} onClick={() => void act(() => start(server.id))}>
            <Play /> Start
          </Button>
        )}
        <Button
          size="sm"
          variant="ghost"
          disabled={busy}
          aria-label={`Uninstall ${server.name}`}
          onClick={() =>
            void act(async () => {
              await uninstall(server.id);
              onRemoved();
            })
          }
        >
          <Trash2 /> Uninstall
        </Button>
      </div>
      <ScrollArea className="min-h-0 flex-1">
        <div className="flex max-w-2xl flex-col gap-4 p-5 text-sm">
          {server.description && <p className="text-muted-foreground">{server.description}</p>}
          {(error || actionError) && (
            <div role="alert" className="rounded-lg border border-destructive/30 bg-destructive/5 p-3">
              <p className="text-xs font-medium text-destructive">{actionError ?? 'The server failed'}</p>
              {error && (
                <pre className="mt-1.5 max-h-48 overflow-auto text-[11px] whitespace-pre-wrap break-words text-destructive/90">{error}</pre>
              )}
            </div>
          )}
          <dl className="grid grid-cols-[90px_1fr] gap-x-3 gap-y-2 text-xs">
            <dt className="text-muted-foreground">{server.url ? 'URL' : 'Command'}</dt>
            <dd className="font-mono break-all">{server.url ?? [server.command, ...server.args].join(' ')}</dd>
            {server.env && Object.keys(server.env).length > 0 && (
              <>
                <dt className="text-muted-foreground">Environment</dt>
                <dd className="font-mono break-all">{Object.keys(server.env).join(', ')}</dd>
              </>
            )}
            <dt className="text-muted-foreground">Source</dt>
            <dd className="break-all">{source}</dd>
          </dl>
          {tools && (
            <div>
              <h3 className="mb-1.5 text-xs font-medium text-muted-foreground">
                Tools · {appNames.size ? `${appNames.size} with a UI` : 'no UI (plain MCP server)'}
              </h3>
              <ul className="flex flex-col divide-y divide-border rounded-lg border border-border">
                {tools.map((t) => (
                  <li key={t.name} className="flex items-center gap-2 px-3 py-1.5 text-xs">
                    <span className="min-w-0 flex-1 truncate font-mono">{t.name}</span>
                    {appNames.has(t.name) && <Badge variant="secondary" className="px-1.5 py-0 text-[10px]">App</Badge>}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      </ScrollArea>
    </>
  );
}

/**
 * Local plugins from `~/AgentArea/plugins`: list, on/off and a preview; works
 * offline. A page a plugin opens itself (`ui.open`) shows full-width instead.
 */
export function PluginsPanel() {
  const plugins = usePlugins((s) => s.plugins);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  // Agents write plugins while the app runs, so the list is refreshable.
  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      await usePlugins.setState({ plugins: await listPlugins() });
    } catch (e) {
      setError(errorText(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const current = selected && plugins.some((p) => p.id === selected) ? selected : (plugins[0]?.id ?? null);
  const plugin = plugins.find((p) => p.id === current) ?? null;

  const toggle = async (p: Plugin) => {
    try {
      await setPluginEnabled(p.id, !p.enabled);
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <ListPane
      heading="Plugins"
      icon={<Puzzle className="size-4 text-muted-foreground" />}
      items={plugins.map((p) => ({
        key: p.id,
        title: p.title,
        subtitle: p.description,
        muted: !p.enabled,
        trailing: <Toggle on={p.enabled} label={`${p.enabled ? 'Disable' : 'Enable'} ${p.title}`} onChange={() => void toggle(p)} />,
      }))}
      selected={current}
      onSelect={setSelected}
      onRefresh={() => void load()}
      loading={loading}
      empty={error ? null : 'No plugins. Add a folder with a plugin.json to ~/AgentArea/plugins.'}
    >
      {error ? (
        <Note title="Couldn’t load plugins" error>
          {error}
        </Note>
      ) : plugin && !plugin.enabled ? (
        <Note title={`${plugin.title} is off`}>Switch it on in the list to open it.</Note>
      ) : plugin ? (
        <PluginFrame key={plugin.id} plugin={plugin} />
      ) : (
        <Note title="No plugin selected">Plugins are small local web pages that can read and message your threads.</Note>
      )}
    </ListPane>
  );
}

/** Small on/off switch. */
function Toggle({ on, label, onChange }: { on: boolean; label: string; onChange: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      title={label}
      onClick={onChange}
      className={cn(
        'relative h-4 w-7 shrink-0 rounded-full transition-colors',
        on ? 'bg-foreground' : 'bg-foreground/15',
      )}
    >
      <span
        className={cn(
          'absolute top-0.5 size-3 rounded-full bg-background shadow-sm transition-transform',
          on ? 'translate-x-3.5' : 'translate-x-0.5',
        )}
      />
    </button>
  );
}
