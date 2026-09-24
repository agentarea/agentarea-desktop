/**
 * Renders one local plugin from the `aa-plugin` scheme in an opaque-origin
 * sandbox (scripts and forms, no same-origin), so it can't touch the app's
 * storage, token or Tauri IPC. Its one channel is the `aa:request` bridge.
 */
import { useEffect, useMemo, useRef } from 'react';
import { isBridgeRequest, runBridgeMethod, type BridgeResponse, type Plugin } from '@/lib/plugins';
import { pluginUrl } from '@/lib/schemes';

export interface PluginFrameProps {
  plugin: Plugin;
  /** page to load instead of the plugin's entry (e.g. its sidebar page) */
  page?: string;
  /** `#hash` handed to the page, e.g. which file to show */
  hash?: string;
  className?: string;
}

export function PluginFrame({ plugin, page, hash, className }: PluginFrameProps) {
  const iframeRef = useRef<HTMLIFrameElement>(null);
  const src = useMemo(() => {
    try {
      return pluginUrl(plugin.id, page ?? plugin.entry) + (hash ? `#${hash}` : '');
    } catch {
      return null; // not running inside Tauri (plain `vite`)
    }
  }, [plugin.id, plugin.entry, page, hash]);

  useEffect(() => {
    const onMessage = async (event: MessageEvent) => {
      const frame = iframeRef.current?.contentWindow;
      // Only our frame's window counts; its origin is always "null" (sandboxed).
      if (!frame || event.source !== frame || !isBridgeRequest(event.data)) return;
      const { id, method, params } = event.data;
      let reply: BridgeResponse;
      try {
        reply = { type: 'aa:response', id, result: await runBridgeMethod(plugin.id, method, params) };
      } catch (e) {
        reply = { type: 'aa:response', id, error: e instanceof Error ? e.message : String(e) };
      }
      // An opaque origin can't be named as a target, hence "*"; the target is
      // still that one window.
      frame.postMessage(reply, '*');
    };
    window.addEventListener('message', onMessage);
    return () => window.removeEventListener('message', onMessage);
  }, [plugin.id]);

  if (!src) {
    return (
      <div className="flex flex-1 items-center justify-center p-6 text-sm text-muted-foreground">
        Plugins need the desktop app runtime.
      </div>
    );
  }
  return (
    <iframe
      ref={iframeRef}
      key={src}
      src={src}
      sandbox="allow-scripts allow-forms"
      title={plugin.title}
      className={className ?? 'h-full min-h-0 w-full flex-1 border-0 bg-background'}
    />
  );
}
