/**
 * Sign-in session: tokens plus what is needed to refresh them.
 *
 * Kept in memory for use and mirrored to the OS keychain (Rust `token_*`
 * commands, one entry per environment) so a restart — including every
 * `tauri dev` rebuild — does not sign you out. The access token lives ~1h;
 * `freshAccessToken()` renews it with the refresh token shortly before expiry.
 */
import { config } from './config';

export interface Tokens {
  accessToken: string;
  refreshToken?: string;
  idToken?: string;
  expiresAt: number; // epoch ms
}

export interface Session extends Tokens {
  /** OAuth client the tokens were issued to (needed to refresh) */
  clientId: string;
  tokenEndpoint: string;
  email: string;
}

let current: Session | null = null;
let refreshing: Promise<string | null> | null = null;
/** called when the session can't be renewed (refresh token revoked/expired) */
let onExpired: (() => void) | null = null;

const key = () => `session:${config.apiBaseUrl}`;

async function invoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke<T>(cmd, args);
}

export function onSessionExpired(fn: () => void) {
  onExpired = fn;
}

/** Hand the live access token to Rust (agent cloud tools use it mid-turn). */
function shareToken() {
  void invoke('set_cloud_token', { apiBase: config.apiBaseUrl, token: current?.accessToken ?? null }).catch(() => {});
}

/**
 * Renew ~90s before expiry even when nothing calls the API: a running agent's
 * cloud tools depend on the token staying fresh while the UI sits idle.
 */
let renewTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleRenewal() {
  if (renewTimer) clearTimeout(renewTimer);
  renewTimer = null;
  if (!current?.refreshToken) return;
  const due = Math.max(current.expiresAt - 90_000 - Date.now(), 5_000);
  renewTimer = setTimeout(() => void freshAccessToken(true), due);
}

export async function setSession(s: Session) {
  current = s;
  shareToken();
  scheduleRenewal();
  await invoke('token_save', { key: key(), value: JSON.stringify(s) }).catch((e) =>
    console.warn('could not save session to keychain', e),
  );
}

/** The saved session for the current environment, if any. */
export async function restoreSession(): Promise<Session | null> {
  try {
    const raw = await invoke<string | null>('token_load', { key: key() });
    current = raw ? (JSON.parse(raw) as Session) : null;
  } catch {
    current = null;
  }
  shareToken();
  scheduleRenewal();
  return current;
}

export async function clearSession() {
  current = null;
  shareToken();
  scheduleRenewal();
  await invoke('token_clear', { key: key() }).catch(() => {});
}

/** Forget the in-memory session only (switching environment keeps its keychain entry). */
export function dropSession() {
  current = null;
  shareToken();
  scheduleRenewal();
}

export function getAccessToken(): string | null {
  return current?.accessToken ?? null;
}

export function currentEmail(): string | null {
  return current?.email ?? null;
}

/**
 * A valid access token, renewed first if it expires within a minute (or when
 * `force`, e.g. after a 401). Concurrent callers share one refresh.
 */
export async function freshAccessToken(force = false): Promise<string | null> {
  if (!current) return null;
  if (!force && current.expiresAt - 60_000 > Date.now()) return current.accessToken;
  if (!current.refreshToken) return force ? null : current.accessToken;
  refreshing ??= refresh().finally(() => {
    refreshing = null;
  });
  return refreshing;
}

async function refresh(): Promise<string | null> {
  const s = current;
  if (!s?.refreshToken) return null;
  const { fetch } = await import('@tauri-apps/plugin-http');
  let res: Response;
  try {
    res = await fetch(s.tokenEndpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({
        grant_type: 'refresh_token',
        refresh_token: s.refreshToken,
        client_id: s.clientId,
      }).toString(),
    });
  } catch {
    return s.accessToken; // offline: keep what we have, try again next call
  }
  if (!res.ok) {
    // 400/401: the refresh token is gone (revoked, rotated elsewhere, expired).
    await clearSession();
    onExpired?.();
    return null;
  }
  const tok = (await res.json()) as { access_token: string; refresh_token?: string; id_token?: string; expires_in?: number };
  await setSession({
    ...s,
    accessToken: tok.access_token,
    // Hydra rotates refresh tokens: the old one is now invalid.
    refreshToken: tok.refresh_token ?? s.refreshToken,
    idToken: tok.id_token ?? s.idToken,
    expiresAt: Date.now() + (tok.expires_in ?? 3600) * 1000,
  });
  return current?.accessToken ?? null;
}
