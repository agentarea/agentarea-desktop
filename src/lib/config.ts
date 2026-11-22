/**
 * AgentArea endpoint config.
 *
 * Only the API base is configured, per environment. Everything auth-related is
 * discovered from the platform's OAuth metadata
 * (`/.well-known/oauth-authorization-server`), and the desktop registers itself
 * as an OAuth client there (RFC 7591) — so no manual client setup anywhere.
 */
export interface Environment {
  id: string;
  label: string;
  /** Platform REST API base; also the OAuth issuer and the token audience */
  apiBaseUrl: string;
}

/**
 * Known AgentArea servers. Any other one, self-hosted included, is entered by
 * URL: every server publishes its OAuth metadata, so nothing else is needed.
 * A platform on localhost is offered only by `tauri dev`.
 */
export const SERVERS: Environment[] = [
  { id: 'ai', label: 'agentarea.ai', apiBaseUrl: 'https://api.agentarea.ai' },
  { id: 'ru', label: 'agentarea.ru', apiBaseUrl: 'https://api.agentarea.ru' },
  ...(import.meta.env.DEV ? [{ id: 'local', label: 'Local', apiBaseUrl: 'http://localhost:8000' }] : []),
];

const SERVER_KEY = 'aa.server';
/** Before servers were URLs: the id of a preset. */
const LEGACY_ENV_KEY = 'aa.env';

export function serverFor(apiBaseUrl: string): Environment {
  const known = SERVERS.find((s) => s.apiBaseUrl === apiBaseUrl);
  if (known) return known;
  const host = new URL(apiBaseUrl).host;
  return { id: host, label: host, apiBaseUrl };
}

/**
 * What a user types ("example.com", "https://api.example.com/") as an API base
 * URL, or null. Plain http only for this machine.
 */
export function parseServerUrl(input: string): string | null {
  const text = input.trim();
  if (!text) return null;
  let url: URL;
  try {
    url = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(text) ? text : `https://${text}`);
  } catch {
    return null;
  }
  const local = url.hostname === 'localhost' || url.hostname === '127.0.0.1';
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && local)) return null;
  return url.origin;
}

export function currentEnv(): Environment {
  try {
    const saved = localStorage.getItem(SERVER_KEY);
    if (saved) return serverFor(saved);
    const legacy = SERVERS.find((s) => s.id === localStorage.getItem(LEGACY_ENV_KEY));
    if (legacy) return legacy;
  } catch {
    /* default below */
  }
  return SERVERS[0];
}

export function saveEnv(apiBaseUrl: string) {
  try {
    localStorage.setItem(SERVER_KEY, apiBaseUrl);
    localStorage.removeItem(LEGACY_ENV_KEY);
  } catch {
    /* best-effort */
  }
}

export const config = {
  get apiBaseUrl() {
    return currentEnv().apiBaseUrl;
  },
  /** Redirect captured by the desktop's loopback listener */
  redirectUri: 'http://127.0.0.1:14321/callback',
  scopes: ['openid', 'offline', 'offline_access'],
};

/** Fixed loopback port — must match the port in `redirectUri` and the Rust server. */
export const LOOPBACK_PORT = 14321;
