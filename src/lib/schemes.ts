/**
 * URLs for the app's own custom URI schemes (registered in src-tauri/src/lib.rs).
 *
 * Untrusted web content — MCP Apps and local plugins — runs on these schemes so
 * it never shares an origin with the main window. The URL form is per platform:
 * macOS/Linux use `aa-sandbox://localhost/…`, Windows (WebView2) and Android
 * `http://aa-sandbox.localhost/…`. Tauri's `convertFileSrc` already knows which,
 * so it builds the base.
 */
import { convertFileSrc } from '@tauri-apps/api/core';

export type AppScheme = 'aa-sandbox' | 'aa-plugin';

/** Base URL with a trailing slash; throws outside Tauri (plain `vite`). */
export function schemeBase(scheme: AppScheme): string {
  return convertFileSrc('', scheme);
}

/**
 * `scheme://host[:port]`, by hand: the URL standard gives custom schemes an
 * opaque ("null") origin, while WebKit reports `aa-sandbox://localhost`.
 */
export function originOf(url: string): string | null {
  return /^[a-z][a-z0-9+.-]*:\/\/[^/?#]+/i.exec(url)?.[0].toLowerCase() ?? null;
}

/** The MCP Apps sandbox proxy page; the Rust side checks `host` against the app's origins. */
export function sandboxUrl(csp: unknown): string {
  const q = new URLSearchParams({ host: window.location.origin });
  if (csp) q.set('csp', JSON.stringify(csp));
  return `${schemeBase('aa-sandbox')}mcp-app-sandbox?${q}`;
}

/** A plugin's entry page: `…/<id>/<entry>`, each segment encoded. */
export function pluginUrl(id: string, entry: string): string {
  const path = entry.split('/').map(encodeURIComponent).join('/');
  return `${schemeBase('aa-plugin')}${encodeURIComponent(id)}/${path}`;
}
