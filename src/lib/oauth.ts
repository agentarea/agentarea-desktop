/**
 * OAuth discovery + dynamic client registration against the platform's
 * authorization server. The registered client id is cached per API base.
 */
import { config } from './config';

export interface AuthServer {
  authorization_endpoint: string;
  token_endpoint: string;
  registration_endpoint: string;
  userinfo_endpoint?: string;
}

const clientKey = () => `aa.oauthClient:${config.apiBaseUrl}`;

async function http() {
  return (await import('@tauri-apps/plugin-http')).fetch;
}

export async function discover(): Promise<AuthServer> {
  const fetch = await http();
  const res = await fetch(`${config.apiBaseUrl}/.well-known/oauth-authorization-server`);
  if (!res.ok) throw new Error(`auth discovery failed (${res.status})`);
  return res.json();
}

/** The cached client id, or a freshly registered one. */
export async function ensureClient(as: AuthServer): Promise<string> {
  try {
    const cached = localStorage.getItem(clientKey());
    if (cached) return cached;
  } catch {
    /* no storage: register every time */
  }
  const fetch = await http();
  const res = await fetch(as.registration_endpoint, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      client_name: 'AgentArea Desktop',
      redirect_uris: [config.redirectUri],
      grant_types: ['authorization_code', 'refresh_token'],
      response_types: ['code'],
      token_endpoint_auth_method: 'none',
      scope: config.scopes.join(' '),
    }),
  });
  if (!res.ok) throw new Error(`client registration failed (${res.status})`);
  const { client_id } = (await res.json()) as { client_id: string };
  try {
    localStorage.setItem(clientKey(), client_id);
  } catch {
    /* best-effort */
  }
  return client_id;
}

/** Drop the cached client, e.g. after the auth server stopped recognising it. */
export function forgetClient() {
  try {
    localStorage.removeItem(clientKey());
  } catch {
    /* best-effort */
  }
}
