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

export const ENVIRONMENTS: Environment[] = [
  { id: 'ru', label: 'agentarea.ru', apiBaseUrl: 'https://api.agentarea.ru' },
  { id: 'local', label: 'Local', apiBaseUrl: 'http://localhost:8000' },
];

const ENV_KEY = 'aa.env';

export function currentEnv(): Environment {
  let id: string | null = null;
  try {
    id = localStorage.getItem(ENV_KEY);
  } catch {
    /* default below */
  }
  return ENVIRONMENTS.find((e) => e.id === id) ?? ENVIRONMENTS[0];
}

export function saveEnv(id: string) {
  try {
    localStorage.setItem(ENV_KEY, id);
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
