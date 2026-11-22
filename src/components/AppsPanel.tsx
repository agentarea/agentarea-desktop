/**
 * Apps: one catalog for everything with a UI — MCP Apps (servers this device
 * runs, the AgentArea workspace's instances) and apps built from a folder in
 * ~/AgentArea/plugins — and a details page for each. Opening an app shows it
 * full-width (App routes it); MCP servers without a UI are listed apart.
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { create } from 'zustand';
import {
  AlertTriangle,
  ArrowLeft,
  ChevronRight,
  Cloud,
  Laptop,
  Pin,
  Play,
  Plus,
  RefreshCw,
  ShieldQuestion,
  Square,
  Trash2,
} from 'lucide-react';
import { AddAppDialog } from '@/components/AddAppDialog';
import { EntityIcon } from '@/components/EntityIcon';
import { usePinnedApps, type AppRef } from '@/lib/pinnedApps';
import { startPaneDrag } from './SplitView';
import { PluginIcon } from '@/components/PluginIcon';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Tabs, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { listMcpApps, type McpApp } from '@/lib/api';
import {
  appsOf,
  serverState,
  useLocalMcp,
  type LocalApp,
  type LocalServer,
  type ServerState,
} from '@/lib/localMcp';
import {
  approvePluginServer,
  listPlugins,
  setPluginEnabled,
  setPluginPlacement,
  reviewPluginServer,
  syncBackendContext,
  usePlugins,
  type BackendStatus,
  type PlacementMode,
  type Plugin,
  type ServerReview,
} from '@/lib/plugins';
import { pluginSection, useSidebarLayout } from '@/lib/sidebarLayout';
import { cn } from '@/lib/utils';
import { useAppStore } from '@/store';

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

type Filter = 'all' | 'device' | 'workspace';

/** What the Apps screen shows: the catalog, or one entry's details (by entry id). */
export const useAppsScreen = create<{ detail: string | null; filter: Filter; show: (id: string | null) => void; setFilter: (f: Filter) => void }>(
  (set) => ({
    detail: null,
    filter: 'all',
    show: (detail) => set({ detail }),
    setFilter: (filter) => set({ filter }),
  }),
);

/** A plugin backend waiting on the user (review, allow, a new version) or one that didn't load. */
export function pluginAttention(p: Plugin, status: BackendStatus | undefined): string | null {
  if (!p.enabled) return null;
  if (p.serverError) return `Backend not loaded: ${p.serverError}`;
  if (!p.server) return null;
  if (p.updateAvailable) return 'A new version of its backend is waiting for your review';
  if (!p.serverApproved) return 'Its backend is waiting for your review';
  return status?.notice ?? null;
}

const lastLine = (text: string | null | undefined) => (text ?? '').trim().split('\n').pop() ?? '';

const failed = (s: LocalServer, starting: Record<string, boolean>, errors: Record<string, string>) =>
  serverState(s, !!starting[s.id], errors[s.id]) === 'error';

/** How many apps need the user, for the sidebar's Apps entry. */
export function useAppsAttention(): number {
  const plugins = usePlugins((s) => s.plugins.filter((p) => pluginAttention(p, s.backends[p.id])).length);
  const servers = useLocalMcp((s) => s.servers.filter((x) => failed(x, s.starting, s.errors)).length);
  return plugins + servers;
}

const cloudKey = (a: McpApp) => `cloud:${a.instance_id}/${a.tool_name}`;
const localKey = (a: LocalApp) => `local:${a.serverId}/${a.toolName}`;
const serverKey = (id: string) => `server:${id}`;

// An app as a self-contained reference: AppView opens it, the sidebar pins
// it, and a split pane holds it.
const localRef = (a: LocalApp): AppRef => ({
  key: localKey(a),
  kind: 'local',
  title: a.title,
  subtitle: a.serverName,
  icons: a.icons,
  seed: localKey(a),
  toolName: a.toolName,
  resourceUri: a.resourceUri,
  requiresInput: a.requiresInput,
  serverId: a.serverId,
});
const cloudRef = (a: McpApp, icons?: string[]): AppRef => ({
  key: cloudKey(a),
  kind: 'cloud',
  title: a.title || a.tool_name,
  subtitle: a.instance_name,
  icons,
  seed: a.instance_id,
  toolName: a.tool_name,
  resourceUri: a.resource_uri,
  requiresInput: a.requires_input,
  instanceId: a.instance_id,
});

type Entry =
  | { kind: 'plugin'; id: string; plugin: Plugin }
  | { kind: 'app'; id: string; app: AppRef; description?: string | null }
  | { kind: 'server'; id: string; server: LocalServer };

const titleOf = (e: Entry) => (e.kind === 'plugin' ? e.plugin.title : e.kind === 'app' ? e.app.title : e.server.name);
const isWorkspace = (e: Entry) => e.kind === 'app' && e.app.kind === 'cloud';
const shows = (filter: Filter, e: Entry) => filter === 'all' || (filter === 'workspace') === isWorkspace(e);

// Installed servers start once per app run when the screen opens; one the user
// stopped stays stopped.
const autoStarted = new Set<string>();

const STATE_LABEL: Record<ServerState, string> = {
  stopped: 'Stopped',
  starting: 'Starting…',
  running: 'Running',
  error: 'Failed',
};

function SourceBadge({ workspace }: { workspace: boolean }) {
  return (
    <Badge variant="outline" className="gap-1 px-1.5 py-0 text-[10.5px] font-normal text-muted-foreground">
      {workspace ? <Cloud /> : <Laptop />}
      {workspace ? 'Workspace' : 'This device'}
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

function EntryIcon({ entry, className }: { entry: Entry; className?: string }) {
  if (entry.kind === 'plugin') {
    const p = entry.plugin;
    return p.icon ? (
      <span className={cn('flex shrink-0 items-center justify-center rounded-lg border border-border bg-secondary/60', className)}>
        <PluginIcon plugin={p} className="size-[55%] text-foreground/75" />
      </span>
    ) : (
      <EntityIcon name={p.title} seed={p.id} className={cn('rounded-lg text-[11px]', className)} />
    );
  }
  const { title, icons, seed } = entry.kind === 'app' ? entry.app : { title: entry.server.name, icons: entry.server.icons, seed: entry.server.id };
  return <EntityIcon name={title} icons={icons} seed={seed} className={cn('rounded-lg text-[11px]', className)} />;
}

/** Open an app full-width (App switches to it). */
function openEntry(e: Entry) {
  if (e.kind === 'plugin') usePlugins.getState().open(e.plugin.id, '');
  else if (e.kind === 'app') usePinnedApps.getState().open(e.app.key, e.app);
}

/** Pin an app to the sidebar, or take it out. Plugins show there unless hidden. */
function useInSidebar(e: Entry): [boolean, () => void] {
  const pinned = usePinnedApps((s) => e.kind === 'app' && s.pinned.some((p) => p.key === e.app.key));
  const hidden = useSidebarLayout((s) => e.kind === 'plugin' && s.hidden.includes(pluginSection(e.plugin.id)));
  if (e.kind === 'plugin') return [!hidden, () => useSidebarLayout.getState().setHidden(pluginSection(e.plugin.id), !hidden)];
  if (e.kind === 'app') return [pinned, () => usePinnedApps.getState().toggle(e.app)];
  return [false, () => {}];
}

function SidebarButton({ entry, compact }: { entry: Entry; compact?: boolean }) {
  const [shown, toggle] = useInSidebar(entry);
  const label = shown ? 'Remove from sidebar' : 'Show in sidebar';
  return (
    <Button
      size={compact ? 'icon-xs' : 'sm'}
      variant={compact ? 'ghost' : 'outline'}
      aria-pressed={shown}
      aria-label={label}
      title={label}
      onClick={(ev) => {
        ev.stopPropagation();
        toggle();
      }}
      className={cn(compact && (shown ? 'text-foreground' : 'text-muted-foreground'))}
    >
      <Pin className={cn(shown && 'fill-current')} />
      {!compact && (shown ? 'In sidebar' : 'Show in sidebar')}
    </Button>
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
      onClick={(e) => {
        e.stopPropagation();
        onChange();
      }}
      className={cn('relative h-4 w-7 shrink-0 rounded-full transition-colors', on ? 'bg-foreground' : 'bg-foreground/15')}
    >
      <span
        className={cn(
          'absolute top-0.5 left-0 size-3 rounded-full bg-background shadow-sm transition-transform',
          on ? 'translate-x-3.5' : 'translate-x-0.5',
        )}
      />
    </button>
  );
}

function Section({
  title,
  count,
  icon,
  action,
  children,
}: {
  title: ReactNode;
  count?: number;
  icon?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section>
      <h2 className="mb-1.5 flex min-h-6 items-center gap-1.5 px-1 text-[12px] font-medium text-muted-foreground">
        {icon}
        {title}
        {!!count && <span className="rounded-full bg-secondary px-1.5 text-[11px] text-foreground">{count}</span>}
        {action && <span className="ml-auto flex items-center gap-1.5">{action}</span>}
      </h2>
      {children}
    </section>
  );
}

export interface AppsPanelProps {
  /** signed in and connected to the control plane; workspace apps come from the API */
  connected: boolean;
}

export function AppsPanel({ connected }: AppsPanelProps) {
  const [cloudApps, setCloudApps] = useState<McpApp[]>([]);
  const [cloudLoading, setCloudLoading] = useState(false);
  const [cloudError, setCloudError] = useState<string | null>(null);
  const [pluginError, setPluginError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const connecting = useAppStore((s) => s.connection === 'connecting');
  const mcps = useAppStore((s) => s.mcps);
  const detail = useAppsScreen((s) => s.detail);
  const filter = useAppsScreen((s) => s.filter);

  const servers = useLocalMcp((s) => s.servers);
  const tools = useLocalMcp((s) => s.tools);
  const starting = useLocalMcp((s) => s.starting);
  const errors = useLocalMcp((s) => s.errors);
  const listError = useLocalMcp((s) => s.listError);
  const plugins = usePlugins((s) => s.plugins);
  const backends = usePlugins((s) => s.backends);

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

  // Agents write plugin folders while the app runs, so the list is re-read here.
  const loadPlugins = useCallback(async () => {
    setPluginError(null);
    try {
      usePlugins.setState({ plugins: await listPlugins() });
      void syncBackendContext();
    } catch (e) {
      setPluginError(errorText(e));
    }
  }, []);

  useEffect(() => {
    if (connected) void loadCloud();
    else setCloudApps([]);
  }, [connected, loadCloud]);

  useEffect(() => {
    void loadPlugins();
  }, [loadPlugins]);

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

  const refresh = () => {
    void useLocalMcp.getState().reload();
    void loadPlugins();
    void loadCloud();
  };

  const iconsOf = (a: McpApp) => mcps.find((m) => m.id === a.instance_id)?.icons;
  const localApps = servers.flatMap((s) => (tools[s.id] ? appsOf(s, tools[s.id]) : []));
  const apps: Entry[] = [
    ...plugins.map((p): Entry => ({ kind: 'plugin', id: pluginSection(p.id), plugin: p })),
    ...localApps.map((a): Entry => ({ kind: 'app', id: localKey(a), app: localRef(a), description: a.description })),
    ...cloudApps.map((a): Entry => ({ kind: 'app', id: cloudKey(a), app: cloudRef(a, iconsOf(a)), description: a.description })),
  ];
  const serverEntries = servers.map((s): Entry => ({ kind: 'server', id: serverKey(s.id), server: s }));
  const all = [...apps, ...serverEntries];
  const open = detail ? all.find((e) => e.id === detail) : undefined;

  if (open) return <AppDetails entry={open} onStopped={(id) => autoStarted.add(id)} />;

  const reviewOf = (e: Entry): string | null =>
    e.kind === 'plugin'
      ? pluginAttention(e.plugin, backends[e.plugin.id])
      : e.kind === 'server' && failed(e.server, starting, errors)
        ? `Failed to start: ${lastLine(errors[e.server.id] ?? e.server.stderr) || 'the server exited'}`
        : null;
  const review = all.filter((e) => reviewOf(e) && shows(filter, e));
  const grid = apps.filter((e) => shows(filter, e) && !review.includes(e));
  const withApps = new Set(localApps.map((a) => a.serverId));
  const toolsOnly = serverEntries.filter(
    (e) => e.kind === 'server' && !withApps.has(e.server.id) && !review.includes(e) && filter !== 'workspace',
  );

  const problems = [
    pluginError && filter !== 'workspace' && `Couldn’t read apps in ~/AgentArea/plugins: ${pluginError}`,
    listError && filter !== 'workspace' && `Couldn’t read MCP servers on this device: ${listError}`,
    cloudError && filter !== 'device' && `Couldn’t load workspace apps: ${cloudError}`,
  ].filter((p): p is string => !!p);
  const workspaceNote =
    filter === 'device' || connected ? null : connecting ? 'Connecting to your workspace…' : 'Sign in to see your workspace’s apps.';
  const startingCount = filter === 'workspace' ? 0 : servers.filter((s) => starting[s.id]).length;

  return (
    <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek">
      <div className="mx-auto flex max-w-5xl flex-col gap-6 px-6 pt-2 pb-10">
        <div className="flex flex-wrap items-center gap-3">
          <Tabs value={filter} onValueChange={(v) => useAppsScreen.getState().setFilter(v as Filter)}>
            <TabsList>
              <TabsTrigger value="all" className="px-3 text-[12.5px]">All</TabsTrigger>
              <TabsTrigger value="device" className="px-3 text-[12.5px]">
                <Laptop className="size-3.5" /> This device
              </TabsTrigger>
              <TabsTrigger value="workspace" className="px-3 text-[12.5px]">
                <Cloud className="size-3.5" /> Workspace
              </TabsTrigger>
            </TabsList>
          </Tabs>
          <span className="flex-1" />
          <Button variant="ghost" size="icon-sm" onClick={refresh} disabled={cloudLoading} aria-label="Refresh" title="Refresh">
            <RefreshCw className={cn('size-3.5', cloudLoading && 'animate-spin')} />
          </Button>
          <button
            onClick={() => setAdding(true)}
            className="flex shrink-0 items-center gap-1 rounded-full bg-foreground px-3 py-1 text-[12px] font-medium text-background"
          >
            <Plus className="size-3.5" /> Add app
          </button>
        </div>

        {review.length > 0 && (
          <Section title="Needs review" count={review.length} icon={<ShieldQuestion className="size-3.5 text-amber-600" />}>
            <div className="flex flex-col overflow-hidden rounded-2xl border border-amber-500/30 bg-amber-500/[0.04]">
              {review.map((e) => (
                <div
                  key={e.id}
                  onClick={() => useAppsScreen.getState().show(e.id)}
                  className="flex cursor-default items-center gap-3 border-b border-amber-500/20 px-3.5 py-2.5 transition-colors last:border-b-0 hover:bg-amber-500/[0.06]"
                >
                  <EntryIcon entry={e} className="size-7" />
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className="truncate text-[13.5px] font-medium">{titleOf(e)}</span>
                    <span className="truncate text-[12px] text-muted-foreground" title={reviewOf(e) ?? undefined}>
                      {reviewOf(e)}
                    </span>
                  </span>
                  <Button size="xs" variant="outline">
                    {e.kind === 'server' ? 'Details' : 'Review…'}
                  </Button>
                </div>
              ))}
            </div>
          </Section>
        )}

        <section className="flex flex-col gap-3">
          {grid.length > 0 ? (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(250px,1fr))] gap-3">
              {grid.map((e) => (
                <AppCard key={e.id} entry={e} description={e.kind === 'plugin' ? e.plugin.description : e.kind === 'app' ? e.description : null} />
              ))}
            </div>
          ) : (
            <div className="rounded-2xl border border-dashed border-border px-6 py-10 text-center">
              <p className="text-[13.5px] font-medium">
                {startingCount ? 'Starting apps on this device…' : filter === 'workspace' ? 'No workspace apps' : 'No apps yet'}
              </p>
              <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted-foreground">
                {filter === 'workspace'
                  ? 'MCP servers in your AgentArea workspace that ship a UI show up here.'
                  : 'Add an app to run it on this device. Apps with a UI from your AgentArea workspace show up here too.'}
              </p>
            </div>
          )}
          {grid.length > 0 && startingCount > 0 && (
            <p className="px-1 text-[12px] text-muted-foreground">
              Starting {startingCount} {startingCount === 1 ? 'server' : 'servers'} on this device…
            </p>
          )}
          {workspaceNote && <p className="px-1 text-[12px] text-muted-foreground">{workspaceNote}</p>}
          {problems.map((p) => (
            <p key={p} role="alert" className="px-1 text-[12px] break-words text-destructive">
              {p}
            </p>
          ))}
        </section>

        {toolsOnly.length > 0 && <ToolsGroup entries={toolsOnly} />}
      </div>
      <AddAppDialog open={adding} onOpenChange={setAdding} />
    </div>
  );
}

function AppCard({ entry, description }: { entry: Entry; description?: string | null }) {
  const plugin = entry.kind === 'plugin' ? entry.plugin : null;
  const off = !!plugin && !plugin.enabled;
  const [error, setError] = useState<string | null>(null);
  const subtitle = entry.kind === 'app' && entry.app.subtitle !== entry.app.title ? entry.app.subtitle : null;
  return (
    <div
      onClick={() => useAppsScreen.getState().show(entry.id)}
      // Drag onto the main area to open it in a split.
      onPointerDown={(ev) =>
        !off &&
        (entry.kind === 'plugin'
          ? startPaneDrag(ev, { kind: 'plugin', id: entry.plugin.id })
          : entry.kind === 'app' && startPaneDrag(ev, { kind: 'app', app: entry.app }))
      }
      className="group flex cursor-default flex-col gap-2.5 rounded-2xl border border-border bg-background p-3.5 transition-colors select-none hover:border-muted-foreground/30 hover:bg-accent/40"
    >
      <div className="flex items-start gap-3">
        <EntryIcon entry={entry} className={cn('size-9', off && 'opacity-50')} />
        <div className="flex min-w-0 flex-1 flex-col gap-1">
          <span className={cn('truncate text-[13.5px] leading-tight font-medium', off && 'text-muted-foreground')}>{titleOf(entry)}</span>
          <span className="flex min-w-0 items-center gap-1.5">
            <SourceBadge workspace={isWorkspace(entry)} />
            {subtitle && <span className="truncate text-[11.5px] text-muted-foreground-subtle">{subtitle}</span>}
            {off && <span className="text-[11.5px] text-muted-foreground-subtle">Off</span>}
          </span>
        </div>
        {plugin && (
          <Toggle
            on={plugin.enabled}
            label={`${plugin.enabled ? 'Turn off' : 'Turn on'} ${plugin.title}`}
            onChange={() => void setPluginEnabled(plugin.id, !plugin.enabled).catch((e) => setError(errorText(e)))}
          />
        )}
      </div>
      <p className={cn('line-clamp-2 min-h-[36px] text-[12.5px] leading-[18px] text-muted-foreground', off && 'opacity-60')}>
        {description || 'No description.'}
      </p>
      {error && <p className="text-[11.5px] break-words text-destructive">{error}</p>}
      <div className="flex items-center gap-1.5">
        {!off && <SidebarButton entry={entry} compact />}
        <span className="flex-1" />
        <Button
          size="xs"
          variant="ghost"
          className="text-muted-foreground"
          onClick={(ev) => {
            ev.stopPropagation();
            useAppsScreen.getState().show(entry.id);
          }}
        >
          Details
        </Button>
        <Button
          size="xs"
          variant="outline"
          disabled={off}
          onClick={(ev) => {
            ev.stopPropagation();
            openEntry(entry);
          }}
        >
          Open
        </Button>
      </div>
    </div>
  );
}

/** Local MCP servers with no app UI (or not running): tools only, kept out of the app grid. */
function ToolsGroup({ entries }: { entries: Entry[] }) {
  const [open, setOpen] = useState(false);
  const starting = useLocalMcp((s) => s.starting);
  const errors = useLocalMcp((s) => s.errors);
  const tools = useLocalMcp((s) => s.tools);
  return (
    <Section
      title={
        <button onClick={() => setOpen(!open)} aria-expanded={open} className="flex items-center gap-1 hover:text-foreground">
          <ChevronRight className={cn('size-3.5 transition-transform', open && 'rotate-90')} />
          Tools for agents
        </button>
      }
      count={entries.length}
    >
      {open && (
        <>
          <p className="mb-2 px-1 text-[12px] text-muted-foreground">
            MCP servers on this device without an app UI: they only give tools. A stopped server's apps show up above once it
            runs.
          </p>
          <div className="flex flex-col overflow-hidden rounded-2xl border border-border">
            {entries.map((e) => {
              if (e.kind !== 'server') return null;
              const s = e.server;
              const state = serverState(s, !!starting[s.id], errors[s.id]);
              return (
                <div
                  key={e.id}
                  onClick={() => useAppsScreen.getState().show(e.id)}
                  className="flex cursor-default items-center gap-2.5 border-b border-border px-3.5 py-2 transition-colors last:border-b-0 hover:bg-accent/50"
                >
                  <EntryIcon entry={e} className="size-5 rounded-md text-[8px]" />
                  <span className="min-w-0 shrink-0 truncate text-[13px]">{s.name}</span>
                  <span className="min-w-0 flex-1 truncate text-[12px] text-muted-foreground-subtle">{s.description}</span>
                  <span className="flex shrink-0 items-center gap-1.5 text-[11.5px] text-muted-foreground">
                    <StateDot state={state} />
                    {state === 'running' && tools[s.id] ? `${tools[s.id].length} tools` : STATE_LABEL[state]}
                  </span>
                </div>
              );
            })}
          </div>
        </>
      )}
    </Section>
  );
}

const PERMISSIONS: Record<string, string> = {
  fs: 'Read files in a folder you choose, and show them in Finder',
  cloud: 'Read your AgentArea workspace’s files',
  state: 'Read and change the Channels network database, and add secrets to the vault',
  exec: 'Run the programs it declares',
};

/** One app's page on the Apps screen: what it is, where it runs, what it may do, and what needs the user. */
function AppDetails({ entry, onStopped }: { entry: Entry; onStopped: (serverId: string) => void }) {
  const servers = useLocalMcp((s) => s.servers);
  const [error, setError] = useState<string | null>(null);
  const back = () => useAppsScreen.getState().show(null);
  const plugin = entry.kind === 'plugin' ? entry.plugin : null;
  const server =
    entry.kind === 'server'
      ? entry.server
      : entry.kind === 'app' && entry.app.kind === 'local'
        ? servers.find((s) => s.id === entry.app.serverId)
        : undefined;
  const description =
    entry.kind === 'plugin' ? entry.plugin.description : entry.kind === 'app' ? entry.description : entry.server.description;
  const launchable = entry.kind !== 'server' && !(plugin && !plugin.enabled);

  return (
    <div className="min-h-0 flex-1 overflow-y-auto scrollbar-sleek">
      <div className="mx-auto flex max-w-3xl flex-col gap-6 px-6 pt-2 pb-10">
        <button
          onClick={back}
          className="-ml-1 flex w-fit items-center gap-1 rounded-md px-1 py-0.5 text-[12.5px] text-muted-foreground transition-colors hover:text-foreground"
        >
          <ArrowLeft className="size-3.5" /> All apps
        </button>

        <div className="flex items-start gap-4">
          <EntryIcon entry={entry} className="size-12 rounded-xl text-[14px]" />
          <div className="flex min-w-0 flex-1 flex-col gap-1.5">
            <h1 className="truncate text-[17px] leading-tight font-semibold">{titleOf(entry)}</h1>
            <div className="flex flex-wrap items-center gap-1.5 text-[12px] text-muted-foreground">
              <SourceBadge workspace={isWorkspace(entry)} />
              {entry.kind === 'app' && <span>{entry.app.subtitle}</span>}
              {entry.kind === 'server' && <span>MCP server</span>}
              {plugin && !plugin.enabled && <span>Off</span>}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {plugin && (
              <label className="flex items-center gap-2 text-[12px] text-muted-foreground">
                {plugin.enabled ? 'On' : 'Off'}
                <Toggle
                  on={plugin.enabled}
                  label={`${plugin.enabled ? 'Turn off' : 'Turn on'} ${plugin.title}`}
                  onChange={() => void setPluginEnabled(plugin.id, !plugin.enabled).catch((e) => setError(errorText(e)))}
                />
              </label>
            )}
            {launchable && <SidebarButton entry={entry} />}
            {launchable && (
              <Button size="sm" onClick={() => openEntry(entry)}>
                Open
              </Button>
            )}
          </div>
        </div>

        {description && <p className="-mt-2 text-[13px] leading-relaxed text-muted-foreground">{description}</p>}
        {error && (
          <p role="alert" className="text-[12px] break-words text-destructive">
            {error}
          </p>
        )}
        {plugin?.serverError && (
          <Problem title="Its backend didn’t load">{plugin.serverError}</Problem>
        )}
        {entry.kind === 'app' && entry.app.requiresInput && (
          <p className="rounded-xl border border-border bg-muted/40 px-3.5 py-2.5 text-[12.5px] text-muted-foreground">
            Its tool has required arguments, which can’t be entered here yet. Use it from a thread instead.
          </p>
        )}

        {plugin && (
          <>
            <Section title="Permissions">
              <div className="flex flex-col overflow-hidden rounded-2xl border border-border text-[12.5px]">
                <Row label="Threads">List your threads, start one and send messages to it</Row>
                {plugin.permissions.map((p) => (
                  <Row key={p} label={p === 'fs' ? 'Files' : p === 'cloud' ? 'Workspace files' : p === 'state' ? 'Network data' : p}>
                    {PERMISSIONS[p] ?? p}
                    {p === 'fs' && (
                      <span className="mt-0.5 block font-mono text-[11.5px] break-all text-muted-foreground">
                        {plugin.root ?? 'No folder chosen yet'}
                      </span>
                    )}
                    {p === 'cloud' && (
                      <span className="mt-0.5 block font-mono text-[11.5px] break-all text-muted-foreground">
                        {plugin.cloudRoot ? plugin.cloudRoot : 'The whole library'}
                      </span>
                    )}
                  </Row>
                ))}
              </div>
            </Section>
            {(plugin.server || plugin.serverApproved || plugin.staleApproval) && <BackendSection plugin={plugin} onError={setError} />}
            <Section title="Manage">
              <div className="flex flex-col gap-1.5 rounded-2xl border border-border px-3.5 py-3 text-[12.5px] text-muted-foreground">
                <p>
                  Turning it off stops serving it and takes it out of the sidebar. To remove it, delete its folder{' '}
                  <code className="font-mono text-[11.5px] break-all text-foreground">~/AgentArea/plugins/{plugin.id}</code>.
                </p>
              </div>
            </Section>
          </>
        )}

        {entry.kind === 'app' && entry.app.kind === 'cloud' && (
          <Section title="Workspace" icon={<Cloud className="size-3.5" />}>
            <div className="flex flex-col overflow-hidden rounded-2xl border border-border text-[12.5px]">
              <Row label="Server">{entry.app.subtitle}</Row>
              <Row label="Tool">
                <span className="font-mono">{entry.app.toolName}</span>
              </Row>
              <Row label="Managed">In your AgentArea workspace; it runs there and is set up there.</Row>
            </div>
          </Section>
        )}

        {server && <ServerSection server={server} onStopped={() => onStopped(server.id)} onRemoved={back} />}
        {entry.kind === 'app' && entry.app.kind === 'local' && !server && (
          <Problem title="Its server is no longer installed" />
        )}
      </div>
    </div>
  );
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[120px_1fr] gap-3 border-b border-border px-3.5 py-2.5 last:border-b-0">
      <span className="text-muted-foreground">{label}</span>
      <span className="min-w-0 break-words">{children}</span>
    </div>
  );
}

function Problem({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div role="alert" className="rounded-xl border border-destructive/30 bg-destructive/5 px-3.5 py-3">
      <p className="flex items-center gap-1.5 text-[12.5px] font-medium text-destructive">
        <AlertTriangle className="size-3.5" /> {title}
      </p>
      {children && (
        <pre className="mt-1.5 max-h-48 overflow-auto text-[11px] whitespace-pre-wrap break-words text-destructive/90">{children}</pre>
      )}
    </div>
  );
}

/** The local MCP server an app comes from: whether it is up (and why not), how it runs, its tools. */
function ServerSection({ server, onStopped, onRemoved }: { server: LocalServer; onStopped: () => void; onRemoved: () => void }) {
  const tools = useLocalMcp((s) => s.tools[server.id]);
  const starting = useLocalMcp((s) => !!s.starting[server.id]);
  const errors = useLocalMcp((s) => s.errors);
  const { start, stop, uninstall } = useLocalMcp.getState();
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const state = serverState(server, starting, errors[server.id]);
  const error = errors[server.id] ?? (server.status === 'error' ? server.stderr || 'The server exited.' : null);
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
    <Section
      title="Server on this device"
      icon={<Laptop className="size-3.5" />}
      action={
        <>
          {state === 'running' || state === 'starting' ? (
            <Button
              size="xs"
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
            <Button size="xs" variant="outline" disabled={busy} onClick={() => void act(() => start(server.id))}>
              <Play /> Start
            </Button>
          )}
          <Button
            size="xs"
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
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {(error || actionError) && <Problem title={actionError ?? 'The server failed'}>{error}</Problem>}
        <div className="flex flex-col overflow-hidden rounded-2xl border border-border text-[12.5px]">
          <Row label="Status">
            <span className="flex items-center gap-1.5">
              <StateDot state={state} /> {STATE_LABEL[state]} · {server.name}
            </span>
          </Row>
          <Row label={server.url ? 'URL' : 'Command'}>
            <span className="font-mono text-[11.5px] break-all">{server.url ?? [server.command, ...server.args].join(' ')}</span>
          </Row>
          {server.env && Object.keys(server.env).length > 0 && (
            <Row label="Environment">
              <span className="font-mono text-[11.5px] break-all">{Object.keys(server.env).join(', ')}</span>
            </Row>
          )}
          <Row label="Source">{source}</Row>
        </div>
        {tools && (
          <div>
            <h3 className="mb-1.5 px-1 text-[12px] font-medium text-muted-foreground">
              Tools · {appNames.size ? `${appNames.size} with a UI` : 'no UI'}
            </h3>
            <ul className="flex flex-col divide-y divide-border overflow-hidden rounded-2xl border border-border">
              {tools.map((t) => (
                <li key={t.name} className="flex items-center gap-2 px-3.5 py-1.5 text-[12px]">
                  <span className="min-w-0 flex-1 truncate font-mono">{t.name}</span>
                  {appNames.has(t.name) && (
                    <Badge variant="secondary" className="px-1.5 py-0 text-[10px]">
                      App
                    </Badge>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </Section>
  );
}

const PLACEMENTS: { mode: PlacementMode; label: string }[] = [
  { mode: 'auto', label: 'Auto' },
  { mode: 'local', label: 'This device' },
  { mode: 'cloud', label: 'Workspace' },
];

/**
 * An app's MCP backend: where it should run (Auto prefers the workspace's
 * instance, then this device), where it runs now and why not, and the user's
 * approval. Nothing runs or is used before the user reviews exactly what the
 * backend is (command, folder, env values, cloud instance) and allows that.
 */
function BackendSection({ plugin, onError }: { plugin: Plugin; onError: (e: string) => void }) {
  const status = usePlugins((s) => s.backends[plugin.id]);
  const [review, setReview] = useState<ServerReview | null>(null);
  const mode = plugin.placement ?? 'auto';
  const local = plugin.server?.local ?? null;
  const cloud = plugin.server?.cloud ?? null;
  const run = (f: () => Promise<void>) => void f().catch((e) => onError(errorText(e)));
  const attention = pluginAttention(plugin, status);
  useEffect(() => setReview(null), [plugin.id]);
  return (
    <Section
      title="Backend"
      action={
        <>
          {(!plugin.serverApproved || plugin.updateAvailable || status?.notice) && (
            <Button size="xs" onClick={() => run(async () => setReview(await reviewPluginServer(plugin.id)))}>
              {plugin.updateAvailable ? 'Review update…' : plugin.serverApproved ? 'Review…' : 'Review and allow…'}
            </Button>
          )}
          {(plugin.serverApproved || plugin.staleApproval) && (
            <Button size="xs" variant="ghost" onClick={() => run(() => approvePluginServer(plugin.id, false))} title="Stop allowing this backend">
              Revoke
            </Button>
          )}
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {attention && !plugin.serverError && (
          <p className="flex items-center gap-1.5 rounded-xl border border-amber-500/30 bg-amber-500/[0.06] px-3.5 py-2.5 text-[12.5px] text-amber-700 dark:text-amber-500">
            <ShieldQuestion className="size-3.5 shrink-0" /> {attention}
          </p>
        )}
        <div className="flex flex-col overflow-hidden rounded-2xl border border-border text-[12.5px]">
          <Row label="Where it runs">
            <div role="radiogroup" aria-label="Where the backend runs" className="flex w-fit rounded-md border border-border p-0.5 text-[12px]">
              {PLACEMENTS.map((p) => (
                <button
                  key={p.mode}
                  type="button"
                  role="radio"
                  aria-checked={mode === p.mode}
                  disabled={(p.mode === 'local' && !local) || (p.mode === 'cloud' && !cloud)}
                  onClick={() => run(() => setPluginPlacement(plugin.id, p.mode))}
                  className={cn(
                    'rounded px-2 py-0.5 transition-colors disabled:opacity-40',
                    mode === p.mode ? 'bg-foreground text-background' : 'hover:bg-accent',
                  )}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </Row>
          {status && (
            <Row label="Now">
              <span className="flex flex-col gap-1">
                <Badge variant={status.resolved === 'unavailable' ? 'destructive' : 'secondary'} className="gap-1">
                  {status.resolved === 'cloud' ? <Cloud /> : status.resolved === 'local' ? <Laptop /> : null}
                  {status.resolved === 'cloud' ? 'Runs in the workspace' : status.resolved === 'local' ? 'Runs on this device' : 'Unavailable'}
                </Badge>
                {status.reason && <span className="text-muted-foreground">{status.reason}</span>}
                {status.notice && <span>{status.notice}</span>}
              </span>
            </Row>
          )}
          <Row label="Allowed">
            {plugin.serverApproved ? (plugin.updateAvailable ? 'An older version; the folder changed since' : 'Yes') : 'Not yet'}
          </Row>
        </div>
        {review && (
          <div className="flex flex-col gap-1.5 rounded-2xl border border-border bg-muted/40 p-3.5 text-xs">
            <p className="text-muted-foreground">
              Allowing runs exactly this version: a read-only copy of the app’s folder as shown here. Later changes to the folder
              don't run until you review and allow them.
            </p>
            {review.command && (
              <>
                <span className="font-medium">Runs on this device, as you, without a sandbox</span>
                <code className="whitespace-pre-wrap break-all rounded bg-background px-2 py-1">{review.command.join(' ')}</code>
                <span>
                  program: <code className="break-all">{review.executable}</code>
                </span>
                <span>
                  from a copy of <code className="break-all">{review.cwd}</code> ({review.files} files, {formatBytes(review.bytes)})
                </span>
                {Object.keys(review.env).length > 0 && (
                  <>
                    <span>with environment:</span>
                    <code className="whitespace-pre-wrap break-all rounded bg-background px-2 py-1">
                      {Object.entries(review.env)
                        .map(([k, v]) => `${k}=${v}`)
                        .join('\n')}
                    </code>
                  </>
                )}
                {review.warnings.map((w) => (
                  <span key={w} className="text-destructive">
                    {w}
                  </span>
                ))}
              </>
            )}
            {review.cloud &&
              (review.cloud.instanceId ? (
                <span>
                  Uses workspace instance <code>{review.cloud.instance}</code> (id <code>{review.cloud.instanceId}</code> in{' '}
                  {review.cloud.workspace}) with your AgentArea sign-in.
                </span>
              ) : (
                <span className="text-muted-foreground">
                  Declares workspace instance <code>{review.cloud.instance}</code>; none in this workspace now (each one is reviewed
                  when it appears).
                </span>
              ))}
            {review.problem && <span className="text-destructive">Can't be allowed: {review.problem}</span>}
            <div className="mt-1 flex gap-2">
              <Button
                size="xs"
                disabled={review.approved || !!review.problem}
                onClick={() =>
                  run(async () => {
                    try {
                      await approvePluginServer(plugin.id, true, review);
                    } finally {
                      setReview(null);
                    }
                  })
                }
              >
                {review.approved ? 'Already allowed' : 'Allow'}
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setReview(null)}>
                Cancel
              </Button>
            </div>
          </div>
        )}
      </div>
    </Section>
  );
}

function formatBytes(n: number): string {
  return n < 1024 ? `${n} B` : n < 1 << 20 ? `${(n / 1024).toFixed(0)} KB` : `${(n / (1 << 20)).toFixed(1)} MB`;
}
