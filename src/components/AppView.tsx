import { useEffect, useMemo, useState } from 'react';
import { Pin, PinOff, X } from 'lucide-react';
import { cn } from '@/lib/utils';
import { McpAppFrame } from '@/components/McpAppFrame';
import { callMcpAppTool, listMcpInstanceTools, readMcpAppResource, type McpAppResource } from '@/lib/api';
import { linkedAppRef, type AppLink } from '@/lib/appLinks';
import { listTools, localToolCaller, readLocalAppResource, useLocalMcp } from '@/lib/localMcp';
import { toCallToolResult, type CallAppTool } from '@/lib/mcp-apps/tool-result';
import { usePinnedApps, type AppRef } from '@/lib/pinnedApps';

/** Cloud apps call their tools through the AgentArea API. */
export const cloudToolCaller =
  (instanceId: string): CallAppTool =>
  async (call) => {
    const r = await callMcpAppTool(instanceId, call);
    return toCallToolResult({ content: r.content, structuredContent: r.structured_content, isError: r.is_error });
  };

/** The view an app link opens: a tool of the same connection, resolved against its tool list. */
async function resolveAppLink(source: AppRef, link: AppLink): Promise<AppRef> {
  const tools =
    source.kind === 'local'
      ? (useLocalMcp.getState().tools[source.serverId!] ?? (await listTools(source.serverId!)))
      : await listMcpInstanceTools(source.instanceId!);
  return linkedAppRef(source, link, tools);
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * One MCP App: header (where it runs, pin to sidebar) and the sandboxed frame.
 * Local apps start their server first if it isn't running, so a pinned app
 * opens with one click even right after launch. In a split pane the pane
 * draws a compact header instead (`chrome={false}`).
 *
 * A view the app opens through an app link goes to `onOpenLinked` (a split
 * puts it in the pane beside); without it, a side panel here shows it, and a
 * new link replaces the panel's view — like the web app.
 */
export function AppView({
  app,
  chrome = true,
  onOpenLinked,
  onClose,
}: {
  app: AppRef;
  chrome?: boolean;
  onOpenLinked?: (linked: AppRef) => void;
  /** shows a close button in the header (the side panel's view) */
  onClose?: () => void;
}) {
  const pinned = usePinnedApps((s) => s.pinned.some((p) => p.key === app.key));
  const toggle = usePinnedApps((s) => s.toggle);
  const [state, setState] = useState<{ key: string; data?: McpAppResource; error?: string } | null>(null);
  const [linked, setLinked] = useState<AppRef | null>(null);
  // A linked view brings the arguments its required inputs need.
  const needsInput = app.requiresInput && !app.args;
  // Stable per app: a new object would restart the running app.
  const entryArgs = useMemo(() => app.args, [app.key]);
  const openAppLink = async (link: AppLink) => (onOpenLinked ?? setLinked)(await resolveAppLink(app, link));

  useEffect(() => {
    if (needsInput) return;
    let cancelled = false;
    setState({ key: app.key });
    (async () => {
      if (app.kind === 'local') {
        const local = useLocalMcp.getState();
        if (local.servers.length === 0) await local.reload();
        const server = useLocalMcp.getState().servers.find((s) => s.id === app.serverId);
        if (!server) throw new Error('This app’s server is no longer installed.');
        if (server.status !== 'running') await useLocalMcp.getState().start(server.id);
        const err = useLocalMcp.getState().errors[server.id];
        if (err) throw new Error(err);
        return readLocalAppResource(app.serverId!, app.resourceUri);
      }
      return readMcpAppResource(app.instanceId!, app.resourceUri);
    })().then(
      (data) => !cancelled && setState({ key: app.key, data }),
      (e) => !cancelled && setState({ key: app.key, error: errorText(e) }),
    );
    return () => {
      cancelled = true;
    };
  }, [app.key, app.resourceUri, needsInput]);

  const current = state?.key === app.key ? state : null;
  const local = app.kind === 'local';

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      {chrome && (
        <div className="flex shrink-0 items-center gap-3 border-b border-border px-4 py-2.5">
          <div className="min-w-0 flex-1">
            <h2 className="truncate text-sm font-medium">{app.title}</h2>
            <p className="truncate text-xs text-muted-foreground">
              {local ? `On this device · ${app.subtitle}` : `AgentArea · ${app.subtitle}`}
            </p>
          </div>
          <button
            onClick={() => toggle(app)}
            className={cn(
              'flex items-center gap-1.5 rounded-full border border-border px-2.5 py-1 text-[12px] transition-colors hover:bg-accent',
              pinned ? 'text-foreground' : 'text-muted-foreground',
            )}
            title={pinned ? 'Unpin from sidebar' : 'Pin to sidebar'}
          >
            {pinned ? <PinOff className="size-3.5" /> : <Pin className="size-3.5" />}
            {pinned ? 'Pinned' : 'Pin'}
          </button>
          <span className="rounded-full border border-border px-2 py-0.5 text-[11px] text-muted-foreground">
            {local ? 'Local' : 'Cloud'}
          </span>
          {onClose && (
            <button
              onClick={onClose}
              aria-label="Close panel"
              title="Close panel"
              className="flex size-6 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
            >
              <X className="size-3.5" />
            </button>
          )}
        </div>
      )}
      <div className="flex min-h-0 min-w-0 flex-1">
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          {needsInput ? (
            <Centered title="This app needs input">
              Its tool has required arguments, which can’t be entered here yet. Use it from a thread instead.
            </Centered>
          ) : current?.error ? (
            <Centered title="The app could not be loaded" error>
              {current.error}
            </Centered>
          ) : current?.data ? (
            <McpAppFrame
              key={current.key}
              callTool={local ? localToolCaller(app.serverId!) : cloudToolCaller(app.instanceId!)}
              toolName={app.toolName}
              title={app.title}
              resource={current.data}
              entryToolArguments={entryArgs}
              onOpenAppLink={openAppLink}
            />
          ) : (
            <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
              {local ? 'Starting app…' : 'Loading app…'}
            </div>
          )}
        </div>
        {linked && (
          <aside className="flex min-h-0 w-[42%] max-w-[720px] min-w-[360px] flex-col border-l border-border">
            <AppView key={linked.key} app={linked} onOpenLinked={setLinked} onClose={() => setLinked(null)} />
          </aside>
        )}
      </div>
    </div>
  );
}

function Centered({ title, children, error }: { title: string; children?: React.ReactNode; error?: boolean }) {
  return (
    <div className="flex flex-1 items-center justify-center p-6">
      <div className="max-w-md rounded-2xl border border-border bg-secondary/40 px-6 py-5 text-center">
        <p className={cn('text-sm font-medium', error && 'text-destructive')}>{title}</p>
        {children && <p className="mt-1 text-[13px] break-words text-muted-foreground">{children}</p>}
      </div>
    </div>
  );
}
