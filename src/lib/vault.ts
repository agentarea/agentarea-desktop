/**
 * Vault secrets (src-tauri/src/vault.rs). Values live in the OS keychain and
 * never come back to JS: the UI can set or replace a value, not read it.
 * Metadata is `~/AgentArea/secrets.json`. Agents use a secret through the
 * local `agentarea_secrets` MCP server, by one-time handle (secrets_mcp.rs).
 */
import { invoke } from '@tauri-apps/api/core';

export interface Secret {
  id: string;
  /** UPPER_SNAKE, unique */
  name: string;
  description?: string;
  /** `api.example.com` or `*.example.com` */
  allowedHosts: string[];
  /** unix ms */
  createdAt: number;
  lastUsedAt?: number;
  /** what the agent said it needed it for, last time */
  lastPurpose?: string;
}

export function listSecrets(): Promise<Secret[]> {
  return invoke<Secret[]>('vault_list');
}

export function createSecret(s: {
  name: string;
  description?: string;
  allowedHosts: string[];
  value: string;
}): Promise<Secret> {
  return invoke<Secret>('vault_create', s);
}

/** Omitted fields stay as they are; an empty description clears it. */
export function updateSecret(
  id: string,
  patch: { description?: string; allowedHosts?: string[]; value?: string },
): Promise<Secret> {
  return invoke<Secret>('vault_update', { id, ...patch });
}

export function deleteSecret(id: string): Promise<void> {
  return invoke<void>('vault_delete', { id });
}

/** "api.github.com, *.example.com" → hosts; https:// and paths are dropped for convenience. */
export function parseHosts(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((h) => h.trim().replace(/^[a-z]+:\/\//i, '').replace(/\/.*$/, ''))
    .filter(Boolean);
}
