import { useEffect, useState } from 'react';
import { Pin, PinOff } from 'lucide-react';
import { cn } from '@/lib/utils';
import { McpAppFrame } from '@/components/McpAppFrame';
import { callMcpAppTool, readMcpAppResource, type McpAppResource } from '@/lib/api';
import { localToolCaller, readLocalAppResource, useLocalMcp } from '@/lib/localMcp';
import { toCallToolResult, type CallAppTool } from '@/lib/mcp-apps/tool-result';
import { usePinnedApps, type AppRef } from '@/lib/pinnedApps';

/** Cloud apps call their tools through the AgentArea API. */
export const cloudToolCaller =
  (instanceId: string): CallAppTool =>
  async (call) => {
    const r = await callMcpAppTool(instanceId, call);
    return toCallToolResult({ content: r.content, structuredContent: r.structured_content, isError: r.is_error });
  };

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * One MCP App: header (where it runs, pin to sidebar) and the sandboxed frame.
 * Local apps start their server first if it isn't running, so a pinned app
 * opens with one click even right after launch.
 */
export function AppView({ app }: { app: AppRef }) {
  const pinned = usePinnedApps((s) => s.pinned.some((p) => p.key === app.key));
  const toggle = usePinnedApps((s) => s.toggle);
  const [state, setState] = useState<{ key: string; data?: McpAppResource; error?: string } | null>(null);

  useEffect(() => {
    if (app.requiresInput) return;
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
  }, [app.key, app.resourceUri, app.requiresInput]);

  const current = state?.key === app.key ? state : null;
  const local = app.kind === 'local';

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
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
      </div>
      {app.requiresInput ? (
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
        />
      ) : (
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          {local ? 'Starting app…' : 'Loading app…'}
        </div>
      )}
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
