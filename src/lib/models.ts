/**
 * Models and reasoning ("thinking") levels a local thread can run with.
 * Claude Code takes aliases or full ids via `--model` and `--effort`; Codex
 * lists what the user's account offers in its own models cache.
 */
import type { LocalRunner } from './local';

export interface ModelOption {
  id: string;
  name: string;
  efforts: string[];
  defaultEffort?: string;
}

const CLAUDE_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'];

export const CLAUDE_MODELS: ModelOption[] = [
  { id: 'opus', name: 'Opus', efforts: CLAUDE_EFFORTS },
  { id: 'sonnet', name: 'Sonnet', efforts: CLAUDE_EFFORTS },
  { id: 'haiku', name: 'Haiku', efforts: CLAUDE_EFFORTS },
  { id: 'claude-fable-5-1', name: 'Fable 5.1', efforts: CLAUDE_EFFORTS },
];

/** Codex models from its cache; empty until Codex has run once on this Mac. */
export async function loadCodexModels(): Promise<ModelOption[]> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const list = await invoke<{ slug: string; name: string; default_effort: string | null; efforts: string[] }[]>(
      'codex_models',
    );
    return list.map((m) => ({ id: m.slug, name: m.name, efforts: m.efforts, defaultEffort: m.default_effort ?? undefined }));
  } catch {
    return [];
  }
}

export function modelsFor(runner: LocalRunner, codex: ModelOption[]): ModelOption[] {
  return runner === 'claude' ? CLAUDE_MODELS : codex;
}
