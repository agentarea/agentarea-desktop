/**
 * OAuth discovery against the platform's authorization server. The desktop
 * signs in as the first-party client `config.clientId`, which the platform
 * registers itself.
 */
import { config } from './config';

export interface AuthServer {
  authorization_endpoint: string;
  token_endpoint: string;
  userinfo_endpoint?: string;
}

export async function discover(): Promise<AuthServer> {
  const fetch = (await import('@tauri-apps/plugin-http')).fetch;
  const res = await fetch(`${config.apiBaseUrl}/.well-known/oauth-authorization-server`);
  if (!res.ok) throw new Error(`auth discovery failed (${res.status})`);
  return res.json();
}
