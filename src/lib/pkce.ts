/** PKCE helpers (Web Crypto, runs in the webview) + authorize-URL builder. */
import { config } from './config';

function base64UrlEncode(bytes: Uint8Array): string {
  let str = '';
  for (const b of bytes) str += String.fromCharCode(b);
  return btoa(str).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function randomToken(): string {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return base64UrlEncode(bytes);
}

export async function challengeFromVerifier(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier));
  return base64UrlEncode(new Uint8Array(digest));
}

export function buildAuthorizeUrl(args: {
  endpoint: string;
  clientId: string;
  challenge: string;
  state: string;
}): string {
  const params = new URLSearchParams({
    response_type: 'code',
    client_id: args.clientId,
    redirect_uri: config.redirectUri,
    scope: config.scopes.join(' '),
    // The API only accepts tokens minted for it; Hydra adds `aud` only on request.
    audience: config.apiBaseUrl,
    code_challenge: args.challenge,
    code_challenge_method: 'S256',
    state: args.state,
  });
  return `${args.endpoint}?${params.toString()}`;
}
