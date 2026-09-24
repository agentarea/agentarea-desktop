/**
 * Renders one MCP App: a tool's `ui://` HTML resource, running in the sandbox
 * proxy on the `aa-sandbox` scheme and talking to us over the ext-apps
 * AppBridge (JSON-RPC over postMessage). Ported from the web app's McpAppFrame;
 * tool calls go through `callTool`, so the same frame serves cloud apps (the
 * AgentArea API) and apps on this device (the local MCP runtime).
 */
import { useEffect, useRef, useState } from 'react';
import {
  AppBridge,
  PostMessageTransport,
  type McpUiHostContext,
  type McpUiResourceCsp,
  type McpUiResourcePermissions,
} from '@modelcontextprotocol/ext-apps/app-bridge';
import type { McpAppResource } from '@/lib/api';
import type { CallAppTool } from '@/lib/mcp-apps/tool-result';
import { MCP_UI_STYLE_TOKEN_NAMES, mcpUiStyleVariablesFromTokens } from '@/lib/mcp-apps/styles';
import { originOf, sandboxUrl } from '@/lib/schemes';
import { useAppStore } from '@/store';

const HOST_INFO = { name: 'AgentArea Desktop', version: '0.1.0' } as const;
const SANDBOX_PROXY_READY = 'ui/notifications/sandbox-proxy-ready';
const PROXY_READY_TIMEOUT_MS = 15_000;
// An app that throws before connecting (a blocked script, a bad bundle) never
// initializes; without a bound the frame would say "Loading" forever.
const APP_INITIALIZE_TIMEOUT_MS = 30_000;
// Stable default: a fresh `{}` per render would restart the app every render.
const NO_ARGUMENTS: Record<string, unknown> = {};

export interface McpAppFrameProps {
  /** reaches the app's MCP server; a new function per render is fine */
  callTool: CallAppTool;
  toolName: string;
  title: string;
  resource: McpAppResource;
  /** arguments for the entry tool call; must be referentially stable */
  entryToolArguments?: Record<string, unknown>;
}

type FailureKind = 'configuration' | 'resource' | 'tool';

const FAILURE_TITLES: Record<FailureKind, string> = {
  configuration: 'MCP Apps are not available here',
  resource: 'The app could not be loaded',
  tool: 'The app’s tool call failed',
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringList(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const result = value.filter((item): item is string => typeof item === 'string');
  return result.length > 0 ? result : undefined;
}

function resourceCsp(value: unknown): McpUiResourceCsp | undefined {
  if (!isRecord(value)) return undefined;
  const csp: McpUiResourceCsp = {
    connectDomains: stringList(value.connectDomains),
    resourceDomains: stringList(value.resourceDomains),
    frameDomains: stringList(value.frameDomains),
    baseUriDomains: stringList(value.baseUriDomains),
  };
  return Object.values(csp).some((domains) => domains !== undefined) ? csp : undefined;
}

function resourcePermissions(value: unknown): McpUiResourcePermissions | undefined {
  if (!isRecord(value)) return undefined;
  const permissions: McpUiResourcePermissions = {};
  for (const key of ['camera', 'microphone', 'geolocation', 'clipboardWrite'] as const) {
    if (isRecord(value[key])) permissions[key] = {};
  }
  return Object.keys(permissions).length > 0 ? permissions : undefined;
}

// The theme is a `.dark` class on an app wrapper, not on <html>, so both the
// theme and the tokens are read at the iframe's position in the tree.
function readHostAppearance(el: Element): Pick<McpUiHostContext, 'theme' | 'styles'> {
  const computed = getComputedStyle(el);
  const tokens = Object.fromEntries(MCP_UI_STYLE_TOKEN_NAMES.map((name) => [name, computed.getPropertyValue(name)]));
  return {
    theme: el.closest('.dark') ? 'dark' : 'light',
    styles: { variables: mcpUiStyleVariablesFromTokens(tokens) },
  };
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function McpAppFrame({ callTool, toolName, title, resource, entryToolArguments = NO_ARGUMENTS }: McpAppFrameProps) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  // Read at call time so a new caller doesn't restart a running app.
  const callToolRef = useRef(callTool);
  callToolRef.current = callTool;
  const bridgeRef = useRef<AppBridge | null>(null);
  const [status, setStatus] = useState<'loading' | 'running' | 'error'>('loading');
  const [failure, setFailure] = useState<{ kind: FailureKind; message: string } | null>(null);
  const theme = useAppStore((s) => s.settings.theme);

  useEffect(() => {
    const iframe = iframeRef.current;
    if (!iframe) return;

    let disposed = false;
    let connected = false;
    let initialized = false;
    setStatus('loading');
    setFailure(null);
    const csp = resourceCsp(resource.csp);
    const permissions = resourcePermissions(resource.permissions);

    const fail = (reason: unknown, kind: FailureKind = 'resource') => {
      if (disposed) return;
      setFailure({ kind, message: errorMessage(reason) });
      setStatus('error');
      console.error('MCP App failed', reason);
    };

    let src: string;
    try {
      src = sandboxUrl(csp);
    } catch {
      fail(new Error('The MCP Apps sandbox needs the desktop app (Tauri) runtime'), 'configuration');
      return;
    }
    const sandboxOrigin = originOf(src);
    if (!sandboxOrigin || sandboxOrigin === window.location.origin) {
      fail(new Error('MCP Apps sandbox must use a different origin'), 'configuration');
      return;
    }

    let cancelProxyReady = () => {};
    const proxyReady = new Promise<void>((resolve, reject) => {
      const onMessage = (event: MessageEvent) => {
        if (
          event.source !== iframe.contentWindow ||
          event.origin.toLowerCase() !== sandboxOrigin ||
          event.data?.method !== SANDBOX_PROXY_READY
        ) {
          return;
        }
        cleanup();
        cancelProxyReady = () => {};
        resolve();
      };
      const timer = window.setTimeout(() => {
        cleanup();
        cancelProxyReady = () => {};
        reject(new Error('Timed out waiting for the MCP Apps sandbox'));
      }, PROXY_READY_TIMEOUT_MS);
      const cleanup = () => {
        window.clearTimeout(timer);
        window.removeEventListener('message', onMessage);
      };
      cancelProxyReady = () => {
        cleanup();
        reject(new Error('MCP Apps sandbox setup was cancelled'));
      };
      window.addEventListener('message', onMessage);
    });

    iframe.src = src;

    const start = async () => {
      await proxyReady;
      if (disposed || !iframe.contentWindow) return;

      const hostContext: McpUiHostContext = {
        ...readHostAppearance(iframe),
        platform: 'desktop',
        displayMode: 'fullscreen',
        availableDisplayModes: ['fullscreen'],
      };
      const bridge = new AppBridge(null, HOST_INFO, { serverTools: {}, openLinks: {} }, { hostContext });
      bridgeRef.current = bridge;

      // A failed call is the app's to handle: it arrives as the JSON-RPC error
      // of its own request, and the app keeps running.
      bridge.oncalltool = (params) =>
        callToolRef.current({ name: params.name, arguments: params.arguments ?? {}, caller: 'app' });
      bridge.onsizechange = ({ height }) => {
        if (height !== undefined) iframe.style.height = `${Math.max(height, 0)}px`;
      };
      // Links open in the system browser, never inside the webview.
      bridge.onopenlink = async ({ url }) => {
        try {
          const target = new URL(url);
          if (target.protocol !== 'http:' && target.protocol !== 'https:') return { isError: true };
          const { openUrl } = await import('@tauri-apps/plugin-opener');
          await openUrl(target.href);
          return {};
        } catch {
          return { isError: true };
        }
      };
      // Only problems reach the host console; an app's info and debug chatter
      // stays inside its own frame's devtools.
      bridge.onloggingmessage = ({ level, data }) => {
        if (level === 'warning') console.warn('[MCP App]', data);
        else if (['error', 'critical', 'alert', 'emergency'].includes(level)) console.error('[MCP App]', data);
      };
      const initializedPromise = new Promise<void>((resolve, reject) => {
        const timer = window.setTimeout(
          () => reject(new Error('The app did not start within 30 seconds')),
          APP_INITIALIZE_TIMEOUT_MS,
        );
        bridge.oninitialized = () => {
          window.clearTimeout(timer);
          initialized = true;
          resolve();
        };
      });

      await bridge.connect(new PostMessageTransport(iframe.contentWindow, iframe.contentWindow));
      connected = true;
      await bridge.sendSandboxResourceReady({ html: resource.html, csp, permissions });
      await initializedPromise;
      if (disposed) return;

      await bridge.sendHostContextChange(readHostAppearance(iframe));
      await bridge.sendToolInput({ arguments: entryToolArguments });
      setStatus('running');

      try {
        const result = await callToolRef.current({ name: toolName, arguments: entryToolArguments, caller: 'host' });
        if (!disposed) await bridge.sendToolResult(result);
      } catch (toolError) {
        const reason = errorMessage(toolError);
        if (!disposed) {
          setFailure({ kind: 'tool', message: reason });
          setStatus('error');
          await bridge.sendToolCancelled({ reason });
        }
      }
    };

    start().catch((e) => fail(e));

    return () => {
      disposed = true;
      cancelProxyReady();
      const bridge = bridgeRef.current;
      bridgeRef.current = null;
      if (!bridge) return;
      void (async () => {
        try {
          if (connected && initialized) await bridge.teardownResource({});
        } catch (teardownError) {
          console.error('MCP App teardown failed', teardownError);
        } finally {
          await bridge.close();
        }
      })();
    };
  }, [entryToolArguments, resource, toolName]);

  // Theme toggles re-send the host's look so the app can follow it.
  useEffect(() => {
    const bridge = bridgeRef.current;
    const iframe = iframeRef.current;
    if (!bridge || !iframe || status !== 'running') return;
    void bridge.sendHostContextChange(readHostAppearance(iframe));
  }, [theme, status]);

  return (
    <div className="relative flex min-h-0 flex-1 flex-col overflow-hidden bg-background">
      <iframe
        ref={iframeRef}
        sandbox="allow-scripts allow-same-origin allow-forms"
        title={title}
        className="h-full min-h-0 w-full flex-1 border-0"
        aria-busy={status === 'loading'}
      />
      {status === 'loading' && (
        <div className="absolute inset-0 flex items-center justify-center bg-background/90 text-sm text-muted-foreground">
          Loading app…
        </div>
      )}
      {status === 'error' && failure && (
        <div className="absolute inset-0 flex items-center justify-center bg-background/95 p-6">
          <div
            role="alert"
            className="max-w-lg rounded-lg border border-destructive/30 bg-destructive/5 p-5 text-sm text-destructive"
          >
            <p className="font-medium">{FAILURE_TITLES[failure.kind]}</p>
            <p className="mt-2 break-words">{failure.message}</p>
          </div>
        </div>
      )}
    </div>
  );
}
